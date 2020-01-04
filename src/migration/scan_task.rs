use super::scan_migration::ScanMigrationTask;
use super::task::{
    AtomicMigrationState, ImportingTask, MgrSubCmd, MigratingTask, MigrationError, MigrationState,
    SwitchArg,
};
use ::common::cluster::{MigrationMeta, MigrationTaskMeta, SlotRange, SlotRangeTag};
use ::common::config::AtomicMigrationConfig;
use ::common::resp_execution::keep_connecting_and_sending_cmd;
use ::common::utils::{pretty_print_bytes, ThreadSafe, NOT_READY_FOR_SWITCHING_REPLY};
use ::common::version::UNDERMOON_MIGRATION_VERSION;
use ::protocol::{RedisClientError, RedisClientFactory, Resp};
use ::proxy::database::DBSendError;
use atomic_option::AtomicOption;
use futures::sync::oneshot;
use futures::{future, Future};
use proxy::backend::{CmdTaskSender, CmdTaskSenderFactory};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

const SCAN_RATE: u64 = 1000;

pub struct RedisScanMigratingTask<RCF: RedisClientFactory, TSF: CmdTaskSenderFactory + ThreadSafe> {
    config: Arc<AtomicMigrationConfig>,
    db_name: String,
    slot_range: (usize, usize),
    meta: MigrationMeta,
    state: Arc<AtomicMigrationState>,
    client_factory: Arc<RCF>,
    redirection_sender_factory: Arc<TSF>,
    stop_signal_sender: AtomicOption<oneshot::Sender<()>>,
    stop_signal_receiver: AtomicOption<oneshot::Receiver<()>>,
    task: ScanMigrationTask,
}

impl<RCF: RedisClientFactory, TSF: CmdTaskSenderFactory + ThreadSafe> ThreadSafe
    for RedisScanMigratingTask<RCF, TSF>
{
}

impl<RCF: RedisClientFactory, TSF: CmdTaskSenderFactory + ThreadSafe>
    RedisScanMigratingTask<RCF, TSF>
{
    pub fn new(
        config: Arc<AtomicMigrationConfig>,
        db_name: String,
        slot_range: (usize, usize),
        meta: MigrationMeta,
        client_factory: Arc<RCF>,
        redirection_sender_factory: Arc<TSF>,
    ) -> Self {
        let (stop_signal_sender, stop_signal_receiver) = oneshot::channel();
        let task = ScanMigrationTask::new(
            meta.src_node_address.clone(),
            meta.dst_node_address.clone(),
            slot_range,
            client_factory.clone(),
            SCAN_RATE,
        );
        Self {
            config,
            meta,
            state: Arc::new(AtomicMigrationState::new()),
            client_factory,
            redirection_sender_factory,
            db_name,
            slot_range,
            stop_signal_sender: AtomicOption::new(Box::new(stop_signal_sender)),
            stop_signal_receiver: AtomicOption::new(Box::new(stop_signal_receiver)),
            task,
        }
    }

    fn send_stop_signal(&self) -> Result<(), MigrationError> {
        if let Some(sender) = self.stop_signal_sender.take(Ordering::SeqCst) {
            sender.send(()).map_err(|()| {
                error!("failed to send stop signal");
                MigrationError::Canceled
            })
        } else {
            Err(MigrationError::AlreadyEnded)
        }
    }

    fn gen_switch_arg(&self, sub_cmd: &str) -> Vec<String> {
        let mut cmd = vec!["UMCTL".to_string(), sub_cmd.to_string()];
        let arg = SwitchArg {
            version: UNDERMOON_MIGRATION_VERSION.to_string(),
            meta: MigrationTaskMeta {
                db_name: self.db_name.clone(),
                slot_range: SlotRange {
                    start: self.slot_range.0,
                    end: self.slot_range.1,
                    tag: SlotRangeTag::Migrating(self.meta.clone()),
                },
            },
        }
        .into_strings();
        cmd.extend(arg.into_iter());
        cmd
    }

    fn pre_check(&self) -> impl Future<Item = (), Error = MigrationError> {
        let state = self.state.clone();
        let meta = self.meta.clone();
        let handle_pre_check = move |resp| -> Result<(), RedisClientError> {
            match resp {
                Resp::Error(err_str) => {
                    if err_str == NOT_READY_FOR_SWITCHING_REPLY.as_bytes() {
                        debug!("pre_check not ready, try again {:?}", meta)
                    } else {
                        error!(
                            "failed to check: {:?}",
                            pretty_print_bytes(err_str.as_slice())
                        );
                    }
                    Ok(())
                }
                reply => {
                    info!("pre_check done");
                    state.set_state(MigrationState::PreBlocking);
                    Err(RedisClientError::Done)
                }
            }
        };

        let client_factory = self.client_factory.clone();
        let dst_proxy_address = self.meta.dst_proxy_address.clone();
        let cmd = self
            .gen_switch_arg("PRECHECK")
            .into_iter()
            .map(|e| e.into_bytes())
            .collect();
        let interval = Duration::from_millis(10);

        keep_connecting_and_sending_cmd(
            client_factory,
            dst_proxy_address,
            cmd,
            interval,
            handle_pre_check,
        )
        .then(|res| match res {
            Ok(()) | Err(RedisClientError::Done) => future::ok(()),
            Err(err) => {
                error!("pre_check error: {:?}", err);
                future::err(MigrationError::RedisClient(err))
            }
        })
    }

    fn pre_block(&self) -> impl Future<Item = (), Error = MigrationError> {
        self.state.set_state(MigrationState::PreSwitch);
        future::ok(())
    }

    fn pre_switch(&self) -> impl Future<Item = (), Error = MigrationError> {
        let state = self.state.clone();
        let meta = self.meta.clone();
        let handle_pre_switch = move |resp| -> Result<(), RedisClientError> {
            match resp {
                Resp::Error(err_str) => {
                    if err_str == NOT_READY_FOR_SWITCHING_REPLY.as_bytes() {
                        debug!("pre_switch not ready, try again {:?}", meta)
                    }
                    error!(
                        "failed to switch: {:?}",
                        pretty_print_bytes(err_str.as_slice())
                    );
                    Ok(())
                }
                reply => {
                    info!("pre_switch done");
                    state.set_state(MigrationState::Scanning);
                    Err(RedisClientError::Done)
                }
            }
        };

        let client_factory = self.client_factory.clone();
        let dst_proxy_address = self.meta.dst_proxy_address.clone();
        let cmd = self
            .gen_switch_arg("PRESWITCH")
            .into_iter()
            .map(|e| e.into_bytes())
            .collect();
        let interval = Duration::from_millis(1);

        keep_connecting_and_sending_cmd(
            client_factory,
            dst_proxy_address,
            cmd,
            interval,
            handle_pre_switch,
        )
        .then(|res| match res {
            Ok(()) | Err(RedisClientError::Done) => future::ok(()),
            Err(err) => {
                error!("pre_switch error: {:?}", err);
                future::err(MigrationError::RedisClient(err))
            }
        })
    }

    fn scan_migrate(&self) -> impl Future<Item = (), Error = MigrationError> {
        let state = self.state.clone();
        let tasks = self
            .task
            .start()
            .ok_or_else(|| MigrationError::AlreadyStarted);
        future::result(tasks).and_then(move |(producer, consumer)| {
            tokio::spawn(
                producer
                    .map(|()| info!("migration producer finished scanning"))
                    .map_err(|err| {
                        error!("migration producer finished error: {:?}", err);
                    }),
            );
            consumer
                .map(move |()| {
                    state.set_state(MigrationState::FinalSwitch);
                    info!("migration consumer finished forwarding data")
                })
                .map_err(|err| {
                    error!("migration consumer finished error: {:?}", err);
                    err
                })
        })
    }

    fn final_switch(&self) -> impl Future<Item = (), Error = MigrationError> {
        let state = self.state.clone();
        let meta = self.meta.clone();
        let handle_final_switch = move |resp| -> Result<(), RedisClientError> {
            match resp {
                Resp::Error(err_str) => {
                    error!(
                        "failed to switch: {:?}",
                        pretty_print_bytes(err_str.as_slice())
                    );
                    Ok(())
                }
                reply => {
                    info!("final_switch done");
                    state.set_state(MigrationState::SwitchCommitted);
                    Err(RedisClientError::Done)
                }
            }
        };

        let client_factory = self.client_factory.clone();
        let dst_proxy_address = self.meta.dst_proxy_address.clone();
        let cmd = self
            .gen_switch_arg("FINALSWITCH")
            .into_iter()
            .map(|e| e.into_bytes())
            .collect();
        let interval = Duration::from_millis(1);

        keep_connecting_and_sending_cmd(
            client_factory,
            dst_proxy_address,
            cmd,
            interval,
            handle_final_switch,
        )
        .then(|res| match res {
            Ok(()) | Err(RedisClientError::Done) => future::ok(()),
            Err(err) => {
                error!("final_switch error: {:?}", err);
                future::err(MigrationError::RedisClient(err))
            }
        })
    }

    fn run(&self) -> impl Future<Item = (), Error = MigrationError> {
        let pre_check = self.pre_check();
        let pre_block = self.pre_block();
        let pre_switch = self.pre_switch();
        let scan_migrate = self.scan_migrate();
        let final_switch = self.final_switch();

        pre_check
            .and_then(move |()| pre_block)
            .and_then(move |()| pre_switch)
            .and_then(move |()| scan_migrate)
            .and_then(move |()| final_switch)
    }
}

impl<RCF: RedisClientFactory, TSF: CmdTaskSenderFactory + ThreadSafe> MigratingTask
    for RedisScanMigratingTask<RCF, TSF>
{
    type Task = <<TSF as CmdTaskSenderFactory>::Sender as CmdTaskSender>::Task;

    fn start(&self) -> Box<dyn Future<Item = (), Error = MigrationError> + Send> {
        let receiver = match self.stop_signal_receiver.take(Ordering::SeqCst) {
            Some(r) => r,
            None => return Box::new(future::err(MigrationError::AlreadyStarted)),
        };
        Box::new(self.run())
    }

    fn stop(&self) -> Box<dyn Future<Item = (), Error = MigrationError> + Send> {
        self.task.stop();
        Box::new(future::result(self.send_stop_signal()))
    }

    fn send(&self, cmd_task: Self::Task) -> Result<(), DBSendError<Self::Task>> {
        if self.state.get_state() == MigrationState::PreCheck {
            return Err(DBSendError::SlotNotFound(cmd_task));
        }

        // TODO: add blocking for PreBlocking

        let redirection_sender = self
            .redirection_sender_factory
            .create(self.meta.src_proxy_address.clone());
        redirection_sender
            .send(cmd_task)
            .map_err(|_e| DBSendError::MigrationError)
    }

    fn get_state(&self) -> MigrationState {
        self.state.get_state()
    }
}

impl<RCF: RedisClientFactory, TSF: CmdTaskSenderFactory + ThreadSafe> Drop
    for RedisScanMigratingTask<RCF, TSF>
{
    fn drop(&mut self) {
        self.send_stop_signal().unwrap_or(())
    }
}

pub struct RedisScanImportingTask<RCF: RedisClientFactory, TSF: CmdTaskSenderFactory + ThreadSafe> {
    config: Arc<AtomicMigrationConfig>,
    meta: MigrationMeta,
    state: Arc<AtomicMigrationState>,
    client_factory: Arc<RCF>,
    redirection_sender_factory: Arc<TSF>,
    stop_signal_sender: AtomicOption<oneshot::Sender<()>>,
    stop_signal_receiver: AtomicOption<oneshot::Receiver<()>>,
}

impl<RCF: RedisClientFactory, TSF: CmdTaskSenderFactory + ThreadSafe> ThreadSafe
    for RedisScanImportingTask<RCF, TSF>
{
}

impl<RCF: RedisClientFactory, TSF: CmdTaskSenderFactory + ThreadSafe>
    RedisScanImportingTask<RCF, TSF>
{
    pub fn new(
        config: Arc<AtomicMigrationConfig>,
        db_name: String,
        meta: MigrationMeta,
        client_factory: Arc<RCF>,
        redirection_sender_factory: Arc<TSF>,
    ) -> Self {
        let (stop_signal_sender, stop_signal_receiver) = oneshot::channel();
        Self {
            config,
            meta: meta.clone(),
            state: Arc::new(AtomicMigrationState::new()),
            client_factory,
            redirection_sender_factory,
            stop_signal_sender: AtomicOption::new(Box::new(stop_signal_sender)),
            stop_signal_receiver: AtomicOption::new(Box::new(stop_signal_receiver)),
        }
    }

    fn send_stop_signal(&self) -> Result<(), MigrationError> {
        if let Some(sender) = self.stop_signal_sender.take(Ordering::SeqCst) {
            sender.send(()).map_err(|()| {
                error!("failed to send stop signal");
                MigrationError::Canceled
            })
        } else {
            Err(MigrationError::AlreadyEnded)
        }
    }
}

impl<RCF: RedisClientFactory, TSF: CmdTaskSenderFactory + ThreadSafe> Drop
    for RedisScanImportingTask<RCF, TSF>
{
    fn drop(&mut self) {
        self.send_stop_signal().unwrap_or(())
    }
}

impl<RCF: RedisClientFactory, TSF: CmdTaskSenderFactory + ThreadSafe> ImportingTask
    for RedisScanImportingTask<RCF, TSF>
{
    type Task = <<TSF as CmdTaskSenderFactory>::Sender as CmdTaskSender>::Task;

    fn start(&self) -> Box<dyn Future<Item = (), Error = MigrationError> + Send> {
        let receiver = match self.stop_signal_receiver.take(Ordering::SeqCst) {
            Some(r) => r,
            None => return Box::new(future::err(MigrationError::AlreadyStarted)),
        };

        Box::new(future::ok(()))
    }

    fn stop(&self) -> Box<dyn Future<Item = (), Error = MigrationError> + Send> {
        Box::new(future::result(self.send_stop_signal()))
    }

    fn send(&self, cmd_task: Self::Task) -> Result<(), DBSendError<Self::Task>> {
        if self.state.get_state() == MigrationState::PreCheck {
            let redirection_sender = self
                .redirection_sender_factory
                .create(self.meta.src_proxy_address.clone());
            return redirection_sender
                .send(cmd_task)
                .map_err(|_e| DBSendError::MigrationError);
        }

        Err(DBSendError::SlotNotFound(cmd_task))
    }

    fn get_state(&self) -> MigrationState {
        self.state.get_state()
    }

    fn handle_switch(
        &self,
        switch_arg: SwitchArg,
        sub_cmd: MgrSubCmd,
    ) -> Result<(), MigrationError> {
        if switch_arg.version != UNDERMOON_MIGRATION_VERSION {
            return Err(MigrationError::IncompatibleVersion);
        }

        match sub_cmd {
            MgrSubCmd::PreCheck => self.state.set_state(MigrationState::PreCheck),
            MgrSubCmd::PreSwitch => self.state.set_state(MigrationState::PreSwitch),
            MgrSubCmd::FinalSwitch => self.state.set_state(MigrationState::SwitchCommitted),
        }
        Ok(())
    }
}