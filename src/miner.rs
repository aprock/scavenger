use burstmath;
use chan;
use config::Cfg;
use futures::sync::mpsc;
use plot::{Plot, SCOOP_SIZE};
use reader::Reader;
use requests::RequestHandler;
use std::cell::RefCell;
use std::collections::HashMap;
use std::fs::read_dir;
use std::path::Path;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use std::u64;
use stopwatch::Stopwatch;
use tokio::prelude::*;
use tokio::timer::Interval;
use tokio_core::reactor::Core;
use utils::get_device_id;
use worker::{create_worker_task, NonceData};

pub struct Miner {
    reader: Reader,
    request_handler: RequestHandler,
    rx_nonce_data: mpsc::Receiver<NonceData>,
    account_id: u64,
    target_deadline: u64,
    state: Arc<Mutex<State>>,
    reader_task_count: usize,
    get_mining_info_interval: u64,
}

pub struct State {
    height: u64,
    best_deadline: u64,
    base_target: u64,
    sw: Stopwatch,

    // count how many reader's scoops have been processed
    processed_reader_tasks: usize,
}

extern "C" {
    pub fn init_shabal_avx2() -> ();

    pub fn init_shabal_avx() -> ();

    pub fn init_shabal_sse2() -> ();
}

impl Miner {
    pub fn new(cfg: Cfg) -> Miner {
        if is_x86_feature_detected!("avx2") {
            println!("using avx2");
            unsafe {
                init_shabal_avx2();
            }
        } else if is_x86_feature_detected!("avx") {
            println!("using avx");
            unsafe {
                init_shabal_avx();
            }
        } else {
            println!("using sse2");
            unsafe {
                init_shabal_sse2();
            }
        }

        let mut drive_id_to_plots: HashMap<String, Arc<Mutex<Vec<RefCell<Plot>>>>> = HashMap::new();
        for plot_dir_str in &cfg.plot_dirs {
            let dir = Path::new(plot_dir_str);
            if !dir.exists() {
                eprintln!("path {} does not exist", plot_dir_str);
                continue;
            }
            if !dir.is_dir() {
                eprintln!("path {} is not a directory", plot_dir_str);
                continue;
            }
            let mut num_plots = 0;
            for file in read_dir(dir).unwrap() {
                let file = &file.unwrap().path();
                if let Ok(p) = Plot::new(file, cfg.use_direct_io) {
                    let drive_id = get_device_id(&file.to_str().unwrap().to_string());
                    let plots = drive_id_to_plots
                        .entry(drive_id)
                        .or_insert(Arc::new(Mutex::new(Vec::new())));
                    plots.lock().unwrap().push(RefCell::new(p));
                    num_plots += 1;
                }
            }
            if num_plots == 0 {
                eprintln!("no plots in {}", plot_dir_str);
            }
        }

        let reader_thread_count = if cfg.reader_thread_count == 0 {
            drive_id_to_plots.len()
        } else {
            cfg.reader_thread_count
        };

        let buffer_count = cfg.worker_thread_count * 2;
        let buffer_size = cfg.nonces_per_cache * SCOOP_SIZE as usize;

        let (tx_empty_buffers, rx_empty_buffers) = chan::sync(buffer_count as usize);
        let (tx_read_replies, rx_read_replies) = chan::sync(buffer_count as usize);

        for _ in 0..buffer_count {
            tx_empty_buffers.send(Arc::new(Mutex::new(vec![0; buffer_size])));
        }

        let (tx_nonce_data, rx_nonce_data) = mpsc::channel(cfg.worker_thread_count);
        for _ in 0..cfg.worker_thread_count {
            thread::spawn(create_worker_task(
                rx_read_replies.clone(),
                tx_empty_buffers.clone(),
                tx_nonce_data.clone(),
            ));
        }

        Miner {
            reader_task_count: drive_id_to_plots.len(),
            reader: Reader::new(
                drive_id_to_plots,
                reader_thread_count,
                rx_empty_buffers,
                tx_read_replies,
            ),
            rx_nonce_data: rx_nonce_data,
            account_id: cfg.account_id,
            target_deadline: cfg.target_deadline,
            request_handler: RequestHandler::new(cfg.url, cfg.secret_phrase),
            state: Arc::new(Mutex::new(State {
                height: 0,
                best_deadline: u64::MAX,
                base_target: 1,
                processed_reader_tasks: 0,
                sw: Stopwatch::new(),
            })),
            get_mining_info_interval: cfg.get_mining_info_interval,
        }
    }

    pub fn run(self) {
        let mut core = Core::new().unwrap();
        let handle = core.handle();
        let request_handler = self.request_handler.clone();

        // you left me no choice!!! at least not one that I could have worked out in two weeks...
        let reader = Rc::new(RefCell::new(self.reader));

        let state = self.state.clone();
        // there might be a way to solve this without two nested moves
        let get_mining_info_interval = self.get_mining_info_interval;
        handle.spawn(
            Interval::new(
                Instant::now(),
                Duration::from_millis(get_mining_info_interval),
            ).for_each(move |_| {
                let state = state.clone();
                let reader = reader.clone();
                request_handler.get_mining_info().then(move |mining_info| {
                    match mining_info {
                        Ok(mining_info) => {
                            let mut state = state.lock().unwrap();
                            if mining_info.height > state.height {
                                state.best_deadline = u64::MAX;
                                state.height = mining_info.height;
                                state.base_target = mining_info.base_target;

                                let gensig =
                                    burstmath::decode_gensig(&mining_info.generation_signature);
                                let scoop = burstmath::calculate_scoop(mining_info.height, &gensig);

                                println!(
                                    "new block: height: {}, scoop: {}",
                                    mining_info.height, scoop
                                );

                                reader.borrow_mut().start_reading(
                                    mining_info.height,
                                    scoop,
                                    Arc::new(gensig),
                                );
                                state.sw.restart();
                                state.processed_reader_tasks = 0;
                            }
                        }
                        _ => eprintln!("error getting mining info"),
                    }
                    future::ok(())
                })
            })
                .map_err(|e| panic!("interval errored; err={:?}", e)),
        );

        let account_id = self.account_id;
        let target_deadline = self.target_deadline;
        let request_handler = self.request_handler.clone();
        let inner_handle = handle.clone();
        let state = self.state.clone();
        let reader_task_count = self.reader_task_count;
        handle.spawn(
            self.rx_nonce_data
                .for_each(move |nonce_data| {
                    let mut state = state.lock().unwrap();
                    let deadline = nonce_data.deadline / state.base_target;
                    if state.best_deadline > deadline && deadline < target_deadline {
                        state.best_deadline = deadline;
                        request_handler.submit_nonce(
                            inner_handle.clone(),
                            account_id,
                            nonce_data.nonce,
                            nonce_data.height,
                            deadline,
                            0,
                        );
                        println!("nonce {} -> deadline: {}", nonce_data.nonce, deadline);
                    }
                    if nonce_data.reader_task_processed {
                        state.processed_reader_tasks += 1;
                        if state.processed_reader_tasks == reader_task_count {
                            println!("round finished after {}ms", state.sw.elapsed_ms());
                        }
                    }
                    Ok(())
                })
                .map_err(|e| panic!("interval errored; err={:?}", e)),
        );

        core.run(future::empty::<(), ()>()).unwrap();
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_new_miner() {}
}
