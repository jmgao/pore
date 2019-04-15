use crate::*;

use std::boxed::FnBox;
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use failure::Error;
use futures::future;
use futures::task::SpawnExt;

fn progress_bar_style(task_count: usize) -> indicatif::ProgressStyle {
  let task_count_digits = task_count.to_string().len();
  let count = "{pos:>".to_owned() + &(6 - task_count_digits).to_string() + "}/{len}";
  let template = "[{elapsed_precise}] {prefix} ".to_owned() + &count + " {bar:40.cyan/blue}: {msg}";
  indicatif::ProgressStyle::default_bar()
    .template(&template)
    .progress_chars("##-")
}

#[derive(Clone, Eq, PartialEq, Ord, PartialOrd)]
struct TaskMonitorId((Instant, String, usize));

struct TaskMonitor {
  counter: AtomicUsize,
  tasks: BTreeSet<TaskMonitorId>,
  progress_bar: Arc<indicatif::ProgressBar>,
}

impl TaskMonitor {
  fn new(progress_bar: Arc<indicatif::ProgressBar>) -> TaskMonitor {
    TaskMonitor {
      counter: AtomicUsize::new(0),
      tasks: BTreeSet::new(),
      progress_bar,
    }
  }

  fn started(&mut self, task_name: String) -> TaskMonitorId {
    let now = Instant::now();
    let counter = self.counter.fetch_add(1, Ordering::Relaxed);
    let task_id = TaskMonitorId((now, task_name, counter));
    self.tasks.insert(task_id.clone());
    self.update_progress_bar();
    task_id
  }

  fn finished(&mut self, task_id: TaskMonitorId) {
    let removed = self.tasks.remove(&task_id);
    assert!(removed);
    self.progress_bar.inc(1);
    self.update_progress_bar();
  }

  fn update_progress_bar(&mut self) {
    match self.tasks.iter().next() {
      Some(task) => {
        self.progress_bar.set_message(&(task.0).1);
      }

      None => {
        self.progress_bar.set_message("");
      }
    }
  }
}

pub struct Pool {
  thread_count: Option<usize>,
  thread_pool: Option<futures::executor::ThreadPool>,
}

pub struct ExecutionResult<T> {
  pub name: String,
  pub result: T,
}

pub struct ExecutionResults<T> {
  pub successful: Vec<ExecutionResult<T>>,
  pub failed: Vec<ExecutionResult<Error>>,
}

impl Pool {
  pub fn with_default_size() -> Pool {
    Pool {
      thread_count: None,
      thread_pool: None,
    }
  }

  pub fn with_size(thread_count: usize) -> Pool {
    Pool {
      thread_count: Some(thread_count),
      thread_pool: None,
    }
  }

  fn thread_pool(&mut self) -> &mut futures::executor::ThreadPool {
    if self.thread_pool.is_some() {
      self.thread_pool.as_mut().unwrap()
    } else {
      let pool = match self.thread_count {
        Some(count) => futures::executor::ThreadPool::builder().pool_size(count).create(),

        None => futures::executor::ThreadPool::new(),
      };

      self.thread_pool = Some(pool.unwrap_or_else(|err| fatal!("failed to create job pool: {}", err)));
      self.thread_pool.as_mut().unwrap()
    }
  }

  pub fn execute<T: Send + 'static>(&mut self, mut job: Job<T>) -> ExecutionResults<T> {
    let task_count = job.tasks.len();
    let pb = Arc::new(indicatif::ProgressBar::new(task_count as u64));
    pb.set_style(progress_bar_style(task_count));
    pb.set_prefix(&job.name);
    pb.enable_steady_tick(1000);

    let task_monitor = Arc::new(Mutex::new(TaskMonitor::new(Arc::clone(&pb))));
    let handles: Vec<_> = job
      .tasks
      .drain(..)
      .map(|task| {
        let task_monitor = Arc::clone(&task_monitor);
        self
          .thread_pool()
          .spawn_with_handle(future::lazy(move |context| {
            let task_id = task_monitor.lock().unwrap().started(task.name.clone());
            let result = (task.task)(&context);
            task_monitor.lock().unwrap().finished(task_id);
            (task.name, result)
          }))
          .expect("failed to spawn job")
      })
      .collect();

    let handles = self.thread_pool().run(future::join_all(handles));
    pb.finish();

    let mut successful = Vec::new();
    let mut failed = Vec::new();

    for (task_name, result) in handles {
      match result {
        Ok(result) => {
          successful.push(ExecutionResult {
            name: task_name,
            result,
          });
        }

        Err(err) => failed.push(ExecutionResult {
          name: task_name,
          result: err,
        }),
      }
    }

    ExecutionResults { successful, failed }
  }
}

pub struct Job<T: Send> {
  name: String,
  tasks: Vec<Task<T>>,
}

impl<T: Send> Job<T> {
  pub fn with_name<N: Into<String>>(name: N) -> Job<T> {
    Job {
      name: name.into(),
      tasks: Vec::new(),
    }
  }

  pub fn add_task<N: Into<String>, F: FnOnce(&futures::task::Context) -> Result<T, Error> + Send + 'static>(
    &mut self,
    name: N,
    task: F,
  ) {
    let name = name.into();
    self.tasks.push(Task {
      name,
      task: Box::new(task),
    });
  }
}

struct Task<T: Send> {
  name: String,
  task: Box<dyn FnBox(&futures::task::Context) -> Result<T, Error> + Send>,
}

#[cfg(test)]
mod tests {
  #[test]
  fn smoke() {
    use super::*;

    let (send_1, recv_1) = std::sync::mpsc::channel();
    let (send_2, recv_2) = std::sync::mpsc::channel();
    let (send_3, recv_3) = std::sync::mpsc::channel();

    let mut job = Job::with_name("forward");
    job.add_task("first", move |_| {
      let data = recv_1.recv().unwrap();
      send_2.send(data + 1).unwrap();
      Ok(data)
    });

    job.add_task("second", move |_| {
      let data = recv_2.recv().unwrap();
      send_3.send(data + 1).unwrap();
      Ok(data)
    });

    job.add_task("third", move |_| {
      let data = recv_3.recv().unwrap();
      Err(format_err!("{}", data + 1))
    });

    send_1.send(0).unwrap();

    let mut pool = Pool::with_size(1);
    let results = pool.execute(job);

    assert_eq!(results.successful.len(), 2);
    assert_eq!(results.successful[0].name, "first");
    assert_eq!(results.successful[0].result, 0);

    assert_eq!(results.successful[1].name, "second");
    assert_eq!(results.successful[1].result, 1);

    assert_eq!(results.failed.len(), 1);
    assert_eq!(results.failed[0].name, "third");
    assert_eq!(format!("{}", results.failed[0].result), format!("{}", 3));
  }
}
