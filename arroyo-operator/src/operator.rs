use crate::context::ArrowContext;
use crate::inq_reader::InQReader;
use crate::{CheckpointCounter, ControlOutcome, SourceFinishType};
use arrow::array::RecordBatch;
use arroyo_metrics::TaskCounters;
use arroyo_rpc::grpc::{TableConfig, TaskCheckpointEventType};
use arroyo_rpc::{ControlMessage, ControlResp};
use arroyo_types::{ArrowMessage, CheckpointBarrier, SignalMessage, Watermark};
use async_trait::async_trait;
use datafusion::execution::FunctionRegistry;
use futures::future::OptionFuture;
use std::any::Any;
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::sync::mpsc::Receiver;
use tokio_stream::StreamExt;
use tracing::{debug, error, info, warn, Instrument};

pub trait OperatorConstructor: Send {
    type ConfigT: prost::Message + Default;
    fn with_config(
        &self,
        config: Self::ConfigT,
        registry: Arc<dyn FunctionRegistry>,
    ) -> anyhow::Result<OperatorNode>;
}

pub enum OperatorNode {
    Source(Box<dyn SourceOperator>),
    Operator(Box<dyn ArrowOperator>),
}

impl OperatorNode {
    pub fn from_source(source: Box<dyn SourceOperator>) -> Self {
        OperatorNode::Source(source)
    }

    pub fn from_operator(operator: Box<dyn ArrowOperator>) -> Self {
        OperatorNode::Operator(operator)
    }

    pub fn name(&self) -> String {
        match self {
            OperatorNode::Source(s) => s.name(),
            OperatorNode::Operator(s) => s.name(),
        }
    }

    pub fn tables(&self) -> HashMap<String, TableConfig> {
        match self {
            OperatorNode::Source(s) => s.tables(),
            OperatorNode::Operator(s) => s.tables(),
        }
    }

    async fn run_behavior(
        &mut self,
        ctx: &mut ArrowContext,
        in_qs: &mut Vec<Receiver<ArrowMessage>>,
    ) -> Option<SignalMessage> {
        match self {
            OperatorNode::Source(s) => {
                s.on_start(ctx).await;

                let result = s.run(ctx).await;

                s.on_close(ctx).await;

                result.into()
            }
            OperatorNode::Operator(o) => operator_run_behavior(o, ctx, in_qs).await,
        }
    }

    pub async fn start(
        mut self: Box<Self>,
        mut ctx: ArrowContext,
        mut in_qs: Vec<Receiver<ArrowMessage>>,
    ) {
        info!(
            "Starting task {}-{}",
            ctx.task_info.operator_name, ctx.task_info.task_index
        );

        let final_message = self.run_behavior(&mut ctx, &mut in_qs).await;

        if let Some(final_message) = final_message {
            ctx.broadcast(ArrowMessage::Signal(final_message)).await;
        }

        info!(
            "Task finished {}-{}",
            ctx.task_info.operator_name, ctx.task_info.task_index
        );

        ctx.control_tx
            .send(ControlResp::TaskFinished {
                operator_id: ctx.task_info.operator_id.clone(),
                task_index: ctx.task_info.task_index,
            })
            .await
            .expect("control response unwrap");
    }
}

async fn run_checkpoint(checkpoint_barrier: CheckpointBarrier, ctx: &mut ArrowContext) -> bool {
    let watermark = ctx.watermarks.last_present_watermark();

    ctx.table_manager
        .checkpoint(checkpoint_barrier, watermark)
        .await;

    ctx.send_checkpoint_event(checkpoint_barrier, TaskCheckpointEventType::FinishedSync)
        .await;

    ctx.broadcast(ArrowMessage::Signal(SignalMessage::Barrier(
        checkpoint_barrier,
    )))
    .await;

    checkpoint_barrier.then_stop
}

#[async_trait]
pub trait SourceOperator: Send + 'static {
    fn name(&self) -> String;

    fn tables(&self) -> HashMap<String, TableConfig> {
        HashMap::new()
    }

    #[allow(unused_variables)]
    async fn on_start(&mut self, ctx: &mut ArrowContext) {}

    async fn run(&mut self, ctx: &mut ArrowContext) -> SourceFinishType;

    #[allow(unused_variables)]
    async fn on_close(&mut self, ctx: &mut ArrowContext) {}

    async fn start_checkpoint(
        &mut self,
        checkpoint_barrier: CheckpointBarrier,
        ctx: &mut ArrowContext,
    ) -> bool {
        ctx.send_checkpoint_event(
            checkpoint_barrier,
            TaskCheckpointEventType::StartedCheckpointing,
        )
        .await;

        run_checkpoint(checkpoint_barrier, ctx).await
    }
}

async fn operator_run_behavior(
    this: &mut Box<dyn ArrowOperator>,
    ctx: &mut ArrowContext,
    in_qs: &mut Vec<Receiver<ArrowMessage>>,
) -> Option<SignalMessage> {
    this.on_start(ctx).await;

    let task_info = ctx.task_info.clone();
    let name = this.name();
    let mut counter = CheckpointCounter::new(in_qs.len());
    let mut closed: HashSet<usize> = HashSet::new();
    let mut sel = InQReader::new();
    let in_partitions = in_qs.len();

    for (i, q) in in_qs.into_iter().enumerate() {
        let stream = async_stream::stream! {
          while let Some(item) = q.recv().await {
            yield(i,item);
          }
        };
        sel.push(Box::pin(stream));
    }
    let mut blocked = vec![];
    let mut final_message = None;

    let mut ticks = 0u64;
    let mut interval =
        tokio::time::interval(this.tick_interval().unwrap_or(Duration::from_secs(60)));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        let operator_future: OptionFuture<_> = this.future_to_poll().into();
        tokio::select! {
            Some(control_message) = ctx.control_rx.recv() => {
                this.handle_controller_message(control_message, ctx).await;
            }

            p = sel.next() => {
                match p {
                    Some(((idx, message), s)) => {
                        let local_idx = idx;

                        debug!("[{}] Handling message {}-{}, {:?}",
                            ctx.task_info.operator_name, 0, local_idx, message);

                        match message {
                            ArrowMessage::Data(record) => {
                                TaskCounters::MessagesReceived.for_task(&ctx.task_info).inc();
                                this.process_batch_index(idx, in_partitions, record, ctx)
                                    .instrument(tracing::trace_span!("handle_fn",
                                        name,
                                        operator_id = task_info.operator_id,
                                        subtask_idx = task_info.task_index)
                                ).await;
                            }
                            ArrowMessage::Signal(signal) => {
                                match this.handle_control_message(idx, &signal, &mut counter, &mut closed, in_partitions, ctx).await {
                                    ControlOutcome::Continue => {}
                                    ControlOutcome::Stop => {
                                        // just stop; the stop will have already been broadcasted for example by
                                        // a final checkpoint
                                        break;
                                    }
                                    ControlOutcome::Finish => {
                                        final_message = Some(SignalMessage::EndOfData);
                                        break;
                                    }
                                    ControlOutcome::StopAndSendStop => {
                                        final_message = Some(SignalMessage::Stop);
                                        break;
                                    }
                                }
                            }
                        }

                        if counter.is_blocked(idx){
                            blocked.push(s);
                        } else {
                            if counter.all_clear() && !blocked.is_empty(){
                                for q in blocked.drain(..){
                                    sel.push(q);
                                }
                            }
                            sel.push(s);
                        }
                    }
                    None => {
                        info!("[{}] Stream completed",ctx.task_info.operator_name);
                        break;
                    }
                }
            }
            Some(val) = operator_future => {
                this.handle_future_result(val, ctx).await;
            }
            _ = interval.tick() => {
                this.handle_tick(ticks, ctx).await;
                ticks += 1;
            }
        }
    }
    this.on_close(&final_message, ctx).await;
    final_message
}

#[async_trait::async_trait]
pub trait ArrowOperator: Send + 'static {
    async fn handle_watermark_int(&mut self, watermark: Watermark, ctx: &mut ArrowContext) {
        // process timers
        tracing::trace!(
            "handling watermark {:?} for {}-{}",
            watermark,
            ctx.task_info.operator_name,
            ctx.task_info.task_index
        );

        if let Watermark::EventTime(_t) = watermark {
            // let finished = ProcessFnUtils::finished_timers(t, ctx).await;
            //
            // for (k, tv) in finished {
            //     self.handle_timer(k, tv.data, ctx).await;
            // }
        }

        if let Some(watermark) = self.handle_watermark(watermark, ctx).await {
            ctx.broadcast(ArrowMessage::Signal(SignalMessage::Watermark(watermark)))
                .await;
        }
    }

    async fn handle_controller_message(
        &mut self,
        control_message: ControlMessage,
        ctx: &mut ArrowContext,
    ) {
        match control_message {
            ControlMessage::Checkpoint(_) => {
                error!("shouldn't receive checkpoint")
            }
            ControlMessage::Stop { .. } => {
                error!("shouldn't receive stop")
            }
            ControlMessage::Commit { epoch, commit_data } => {
                self.handle_commit(epoch, &commit_data, ctx).await;
            }
            ControlMessage::LoadCompacted { compacted } => {
                ctx.load_compacted(compacted).await;
            }
            ControlMessage::NoOp => {}
        }
    }

    async fn handle_control_message(
        &mut self,
        idx: usize,
        message: &SignalMessage,
        counter: &mut CheckpointCounter,
        closed: &mut HashSet<usize>,
        in_partitions: usize,
        ctx: &mut ArrowContext,
    ) -> ControlOutcome {
        match message {
            SignalMessage::Barrier(t) => {
                debug!(
                    "received barrier in {}-{}-{}-{}",
                    self.name(),
                    ctx.task_info.operator_id,
                    ctx.task_info.task_index,
                    idx
                );

                if counter.all_clear() {
                    ctx.control_tx
                        .send(ControlResp::CheckpointEvent(arroyo_rpc::CheckpointEvent {
                            checkpoint_epoch: t.epoch,
                            operator_id: ctx.task_info.operator_id.clone(),
                            subtask_index: ctx.task_info.task_index as u32,
                            time: SystemTime::now(),
                            event_type: TaskCheckpointEventType::StartedAlignment,
                        }))
                        .await
                        .unwrap();
                }

                if counter.mark(idx, &t) {
                    debug!(
                        "Checkpointing {}-{}-{}",
                        self.name(),
                        ctx.task_info.operator_id,
                        ctx.task_info.task_index
                    );

                    ctx.send_checkpoint_event(*t, TaskCheckpointEventType::StartedCheckpointing)
                        .await;

                    self.handle_checkpoint(*t, ctx).await;

                    ctx.send_checkpoint_event(*t, TaskCheckpointEventType::FinishedOperatorSetup)
                        .await;

                    if run_checkpoint(*t, ctx).await {
                        return ControlOutcome::Stop;
                    }
                }
            }
            SignalMessage::Watermark(watermark) => {
                debug!(
                    "received watermark {:?} in {}-{}",
                    watermark,
                    self.name(),
                    ctx.task_info.task_index
                );

                let watermark = ctx
                    .watermarks
                    .set(idx, *watermark)
                    .expect("watermark index is too big");

                if let Some(watermark) = watermark {
                    if let Watermark::EventTime(_t) = watermark {
                        // TOOD: pass to table_manager
                    }

                    self.handle_watermark_int(watermark, ctx).await;
                }
            }
            SignalMessage::Stop => {
                closed.insert(idx);
                if closed.len() == in_partitions {
                    return ControlOutcome::StopAndSendStop;
                }
            }
            SignalMessage::EndOfData => {
                closed.insert(idx);
                if closed.len() == in_partitions {
                    return ControlOutcome::Finish;
                }
            }
        }
        ControlOutcome::Continue
    }

    fn name(&self) -> String;

    fn tables(&self) -> HashMap<String, TableConfig> {
        HashMap::new()
    }

    fn tick_interval(&self) -> Option<Duration> {
        None
    }

    #[allow(unused_variables)]
    async fn on_start(&mut self, ctx: &mut ArrowContext) {}

    async fn process_batch_index(
        &mut self,
        _index: usize,
        _in_partitions: usize,
        batch: RecordBatch,
        ctx: &mut ArrowContext,
    ) {
        self.process_batch(batch, ctx).await
    }

    async fn process_batch(&mut self, batch: RecordBatch, ctx: &mut ArrowContext);

    fn future_to_poll(
        &mut self,
    ) -> Option<Pin<Box<dyn Future<Output = Box<dyn Any + Send>> + Send>>> {
        None
    }

    #[allow(unused_variables)]
    async fn handle_future_result(&mut self, result: Box<dyn Any + Send>, ctx: &mut ArrowContext) {}

    #[allow(unused_variables)]
    async fn handle_timer(&mut self, key: Vec<u8>, value: Vec<u8>, ctx: &mut ArrowContext) {}

    async fn handle_watermark(
        &mut self,
        watermark: Watermark,
        _ctx: &mut ArrowContext,
    ) -> Option<Watermark> {
        Some(watermark)
    }

    #[allow(unused_variables)]
    async fn handle_checkpoint(&mut self, b: CheckpointBarrier, ctx: &mut ArrowContext) {}

    #[allow(unused_variables)]
    async fn handle_commit(
        &mut self,
        epoch: u32,
        commit_data: &HashMap<char, HashMap<u32, Vec<u8>>>,
        ctx: &mut ArrowContext,
    ) {
        warn!("default handling of commit with epoch {:?}", epoch);
    }

    #[allow(unused_variables)]
    async fn handle_tick(&mut self, tick: u64, ctx: &mut ArrowContext) {}

    #[allow(unused_variables)]
    async fn on_close(&mut self, final_mesage: &Option<SignalMessage>, ctx: &mut ArrowContext) {}
}
