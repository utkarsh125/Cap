use flume::Receiver;
use futures::pin_mut;
use indexmap::IndexMap;
use std::{
    thread::{self, JoinHandle},
    time::Duration,
};
use tokio::sync::oneshot;
use tracing::{error, info, trace};

use crate::pipeline::{
    clock::CloneFrom,
    control::ControlBroadcast,
    task::{PipelineReadySignal, PipelineSinkTask, PipelineSourceTask},
    MediaError, Pipeline, PipelineClock,
};

struct Task {
    ready_signal: Receiver<Result<(), MediaError>>,
    join_handle: JoinHandle<()>,
    done_rx: tokio::sync::oneshot::Receiver<Result<(), String>>,
}

pub struct PipelineBuilder<T> {
    clock: T,
    control: ControlBroadcast,
    tasks: IndexMap<String, Task>,
}

impl<T> PipelineBuilder<T> {
    pub fn new(clock: T) -> Self {
        Self {
            clock,
            control: ControlBroadcast::default(),
            tasks: IndexMap::new(),
        }
    }

    pub fn source<O: Send + 'static, C: CloneFrom<T> + Send + 'static>(
        mut self,
        name: impl Into<String>,
        mut task: impl PipelineSourceTask<Clock = C> + 'static,
    ) -> PipelinePathBuilder<T, O> {
        let name = name.into();
        let (output, next_input) = flume::bounded(task.queue_size());
        let clock = C::clone_from(&self.clock);
        let control_signal = self.control.add_listener(name.clone());

        self.spawn_task(name, move |ready_signal| {
            task.run(clock, ready_signal, control_signal);
            Ok(())
        });

        PipelinePathBuilder {
            pipeline: self,
            next_input,
        }
    }

    pub fn spawn_source<C: CloneFrom<T> + Send + 'static>(
        &mut self,
        name: impl Into<String>,
        mut task: impl PipelineSourceTask<Clock = C> + 'static,
    ) {
        let name = name.into();
        let clock = C::clone_from(&self.clock);
        let control_signal = self.control.add_listener(name.clone());

        self.spawn_task(name, move |ready_signal| {
            task.run(clock, ready_signal, control_signal);
            Ok(())
        });
    }

    pub fn spawn_task(
        &mut self,
        name: impl Into<String>,
        launch: impl FnOnce(PipelineReadySignal) -> Result<(), String> + Send + 'static,
    ) {
        let name = name.into();

        if self.tasks.contains_key(&name) {
            panic!("A task with the name {name} has already been added to the pipeline");
        }

        let (ready_sender, ready_signal) = flume::bounded(1);

        let dispatcher = tracing::dispatcher::get_default(|d| d.clone());
        let span = tracing::error_span!("pipeline", task = &name);

        let (done_tx, done_rx) = tokio::sync::oneshot::channel::<Result<(), String>>();

        let join_handle = thread::spawn({
            let name = name.clone();
            move || {
                tracing::dispatcher::with_default(&dispatcher, || {
                    let result = span
                        .in_scope(|| {
                            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                info!("launching task '{name}'");
                                let res = launch(ready_sender);
                                info!("task '{name}' done");
                                res
                            }))
                            .map_err(|e| {
                                if let Some(s) = e.downcast_ref::<&'static str>() {
                                    format!("Panicked: {s}")
                                } else if let Some(s) = e.downcast_ref::<String>() {
                                    format!("Panicked: {s}")
                                } else {
                                    format!("Panicked: Unknown error")
                                }
                            })
                        })
                        .and_then(|v| v);
                    let _ = done_tx.send(result);
                });
            }
        });

        self.tasks.insert(
            name,
            Task {
                ready_signal,
                join_handle,
                done_rx,
            },
        );
    }
}

impl<T: PipelineClock> PipelineBuilder<T> {
    pub async fn build(
        self,
    ) -> Result<(Pipeline<T>, oneshot::Receiver<Result<(), String>>), MediaError> {
        let Self {
            clock,
            control,
            tasks,
        } = self;

        if tasks.is_empty() {
            return Err(MediaError::EmptyPipeline);
        }

        let mut task_handles = IndexMap::new();

        let mut stop_rx = vec![];
        let mut task_names = vec![];

        // TODO: Shut down tasks if launch failed.
        for (name, task) in tasks.into_iter() {
            // TODO: Wait for these in parallel?
            tokio::time::timeout(Duration::from_secs(5), task.ready_signal.recv_async())
                .await
                .map_err(|_| MediaError::TaskLaunch(format!("task timed out: '{name}'")))?
                .map_err(|e| MediaError::TaskLaunch(format!("{name} build / {e}")))??;

            task_handles.insert(name.clone(), task.join_handle);
            stop_rx.push(task.done_rx);
            task_names.push(name);
        }

        tokio::time::sleep(Duration::from_millis(10)).await;

        let (done_tx, done_rx) = oneshot::channel();

        tokio::spawn(async move {
            let (result, index, _) = futures::future::select_all(stop_rx).await;
            let task_name = &task_names[index];

            let result = match result {
                Ok(Err(error)) => Err(format!("Task '{task_name}' failed: {error}")),
                Err(_) => Err(format!("Task '{task_name}' failed for unknown reason")),
                _ => Ok(()),
            };

            if let Err(e) = &result {
                error!("{e}");
            }

            let _ = done_tx.send(result);
        });

        Ok((
            Pipeline {
                clock,
                control,
                task_handles,
                is_shutdown: false,
            },
            done_rx,
        ))
    }
}

pub struct PipelinePathBuilder<Clock, PreviousOutput: Send> {
    pipeline: PipelineBuilder<Clock>,
    next_input: Receiver<PreviousOutput>,
}
