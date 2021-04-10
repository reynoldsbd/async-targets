//! The `async-jobs` crate provides a framework for describing and executing a collection of
//! interdependent and asynchronous jobs. It is intended to be used as the scheduling backbone for
//! programs such as build systems which need to orchestrate arbitrary collections of tasks with
//! complex dependency graphs.
//!
//! The main way to use this crate is by creating implementations of the `IntoJob` trait to describe
//! the tasks in your program and how they depend on one another. To run your jobs, create a
//! `Scheduler` and pass a job to its `run` method.
//!
//! # Example
//!
//! ```
//! # use async_jobs::{Error, IntoJob, Outcome, Job, PlanBuilder, Scheduler};
//! #[derive(PartialEq)]
//! struct Foo(usize);
//!
//! impl IntoJob<()> for Foo {
//!     fn into_job(&self) -> Job<()> {
//!         let num = self.0;
//!         Box::new(move || Box::pin(async move {
//!             println!("foo: {}", num);
//!             Ok(Outcome::Success)
//!         }))
//!     }
//! }
//!
//! #[derive(PartialEq)]
//! struct Bar(usize);
//!
//! impl IntoJob<()> for Bar {
//!     fn plan(&self, plan: &mut PlanBuilder<()>) -> Result<(), Error<()>> {
//!         plan.add_dependency(Foo(self.0 + 1))?;
//!         plan.add_dependency(Foo(self.0 + 2))?;
//!         Ok(())
//!     }
//!
//!     fn into_job(&self) -> Job<()> {
//!         let num = self.0;
//!         Box::new(move || Box::pin(async move {
//!             println!("bar: {}", num);
//!             Ok(Outcome::Success)
//!         }))
//!     }
//! }
//!
//! let sched = Scheduler::default();
//! async_std::task::block_on(sched.run(Bar(7)));
//! ```

use std::collections::HashSet;
use std::future::Future;
use std::iter::FromIterator;
use std::mem;
use std::pin::Pin;
use std::rc::Rc;

use downcast_rs::{impl_downcast, Downcast};

/// Possible job outcomes
#[non_exhaustive]
pub enum Outcome {
    /// Job completed successfully
    Success,
}

/// Errors returned by job scheduler
#[derive(Debug, PartialEq, Eq)]
pub enum Error<E> {
    /// Dependency cycle detected while generating job plan
    Cycle,

    /// One or more jobs failed while executing job plan
    Failed(Vec<E>),

    /// Arbitrary error returned by an implementation of `IntoJob::plan`
    Plan(E),
}

/// Unit of asynchronous work
pub type Job<E> = Box<dyn FnOnce() -> Pin<Box<dyn Future<Output = Result<Outcome, E>>>>>;

/// Information needed to schedule and execute a job
pub trait IntoJob<E>: Downcast {
    /// Configures the job plan with information about this job, such as its dependencies.
    fn plan(&self, plan: &mut PlanBuilder<E>) -> Result<(), Error<E>> {
        // This default impl does not use the plan parameter, but we still want it to be named
        // "plan" in documentation.
        #![allow(unused_variables)]

        Ok(())
    }

    /// Converts this instance into a `Job`.
    fn into_job(&self) -> Job<E>;
}

impl_downcast!(IntoJob<E>);

/// Bookkeeping for individual job during planning
struct PlanBuilderEntry<E> {
    job: Rc<dyn IntoJob<E>>,
    dependencies: Vec<usize>,
    dependents: Vec<usize>,
}

/// An "under construction" execution plan
pub struct PlanBuilder<E> {
    jobs: Vec<PlanBuilderEntry<E>>,
    ancestors: HashSet<usize>,
    current_parent: usize,
    ready: Vec<usize>,
}

impl<E: 'static> PlanBuilder<E> {
    /// Checks for a matching entry in `self.jobs` and returns its index.
    fn index_of<J: IntoJob<E> + PartialEq>(&self, job: &J) -> Option<usize> {
        for (idx, entry) in self.jobs.iter().enumerate() {
            if let Some(existing_job) = entry.job.downcast_ref::<J>() {
                if job == existing_job {
                    return Some(idx);
                }
            }
        }

        None
    }

    /// Adds `job` to the job plan as a dependency of the current job.
    pub fn add_dependency<J: IntoJob<E> + PartialEq>(&mut self, job: J) -> Result<(), Error<E>> {
        // This is where the magic happens. This method performs a *partial* topological sort (aka
        // "dependency resolution" or "dependency ordering") of `job` and all its dependencies. We
        // say *partial* because instead of a complete ordering of jobs this implementation produces
        // a "ready queue" (`self.ready`) containing only the jobs which are currently ready to run.
        //
        // The reason for this difference is that it simplifies the implementation of parallel job
        // scheduling. When the scheduler has capacity to run a new job, the next one is simply
        // pulled from the ready queue without needing to iterate over the full list of jobs to
        // check dependencies.

        // If this job is already part of the job plan, get its index and add it
        // as a dependency of the current parent.
        if let Some(idx) = self.index_of(&job) {
            if self.ancestors.contains(&idx) {
                return Err(Error::Cycle);
            }

            self.jobs[idx].dependents.push(self.current_parent);
            self.jobs[self.current_parent].dependencies.push(idx);
            return Ok(());
        }

        // If we haven't seen this job before, add an entry for it. Then call plan() recursively to
        // get its dependencies and other job information.

        let idx = self.jobs.len();
        let job = Rc::new(job);
        self.jobs.push(PlanBuilderEntry {
            job: job.clone(),
            dependencies: vec![],
            dependents: vec![self.current_parent],
        });
        self.jobs[self.current_parent].dependencies.push(idx);

        self.ancestors.insert(idx);
        let prev_parent = mem::replace(&mut self.current_parent, idx);
        job.plan(self)?;
        self.current_parent = prev_parent;
        self.ancestors.remove(&idx);

        if self.jobs[idx].dependencies.is_empty() {
            self.ready.push(idx);
        }

        Ok(())
    }
}

/// Possible state of a job during execution
enum State<E> {
    Pending(Job<E>),
    Running,
    Success(Outcome),
    Failed(E),
}

impl<E> State<E> {
    /// Returns `true` if this is an instance of `Success`.
    fn success(&self) -> bool {
        match self {
            State::Success(_) => true,
            _ => false,
        }
    }
}

/// Bookkeeping for individual job during execution
struct PlanEntry<E> {
    state: State<E>,
    dependencies: Vec<usize>,
    dependents: Vec<usize>,
}

/// A ready-to-execute job execution plan
struct Plan<E> {
    jobs: Vec<PlanEntry<E>>,
    ready: Vec<usize>,
}

impl<E> Plan<E> {
    /// Creates a new plan for executing `job` and its dependencies.
    fn new<J: IntoJob<E>>(job: J) -> Result<Self, Error<E>> {
        let job = Rc::new(job);

        let mut builder = PlanBuilder {
            jobs: vec![PlanBuilderEntry {
                job: job.clone(),
                dependencies: vec![],
                dependents: vec![],
            }],
            ancestors: HashSet::from_iter([0].iter().cloned()),
            current_parent: 0,
            ready: vec![],
        };

        job.plan(&mut builder)?;
        if builder.jobs[0].dependencies.is_empty() {
            builder.ready.push(0);
        }

        Ok(Self {
            jobs: builder
                .jobs
                .drain(..)
                .map(|e| PlanEntry {
                    state: State::Pending(e.job.into_job()),
                    dependencies: e.dependencies,
                    dependents: e.dependents,
                })
                .collect(),
            ready: builder.ready,
        })
    }

    /// Returns the next job from the ready queue, along with its index
    fn next_job(&mut self) -> Option<(Job<E>, usize)> {
        if self.ready.len() == 0 {
            return None;
        }

        let idx = self.ready.remove(0);
        let state = mem::replace(&mut self.jobs[idx].state, State::Running);

        if let State::Pending(job) = state {
            Some((job, idx))
        } else {
            panic!("unexpected job status")
        }
    }

    /// Marks a job as completed and updates the ready queue with any new jobs that
    /// are now ready to execute as a result.
    fn mark_complete(&mut self, job_idx: usize, res: Result<Outcome, E>) {
        self.jobs[job_idx].state = match res {
            Ok(outcome) => State::Success(outcome),
            Err(err) => State::Failed(err),
        };

        for dep_idx in &self.jobs[job_idx].dependents {
            let is_ready = self.jobs[*dep_idx]
                .dependencies
                .iter()
                .all(|i| self.jobs[*i].state.success());
            if is_ready {
                self.ready.push(*dep_idx);
            }
        }
    }
}

/// Schedules execution of jobs and dependencies
///
/// Uses the builder pattern to configure various aspects of job execution.
#[derive(Default)]
pub struct Scheduler(());

impl Scheduler {
    /// Executes `job` and its dependencies
    pub async fn run<E, J: IntoJob<E>>(&self, job: J) -> Result<(), Error<E>> {
        let mut plan = Plan::new(job)?;

        while let Some((job, idx)) = plan.next_job() {
            plan.mark_complete(idx, job().await);
        }

        let mut errs = vec![];
        for job in plan.jobs {
            if let State::Failed(err) = job.state {
                errs.push(err);
            }
        }

        if errs.len() > 0 {
            Err(Error::Failed(errs))
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {

    use async_std::sync::Mutex;

    use super::*;

    type TestGraph = Vec<(bool, Vec<usize>)>;

    struct TestJob {
        index: usize,
        graph: Rc<TestGraph>,
        trace: Rc<Mutex<Vec<usize>>>,
        success: bool,
    }

    impl IntoJob<usize> for TestJob {
        fn plan(&self, plan: &mut PlanBuilder<usize>) -> Result<(), Error<usize>> {
            for index in &self.graph[self.index].1 {
                plan.add_dependency(TestJob {
                    index: *index,
                    graph: self.graph.clone(),
                    trace: self.trace.clone(),
                    success: self.graph[*index].0,
                })?;
            }

            Ok(())
        }

        fn into_job(&self) -> Job<usize> {
            let trace = self.trace.clone();
            let success = self.success;
            let index = self.index;
            Box::new(move || {
                Box::pin(async move {
                    trace.lock().await.push(index);
                    if success {
                        Ok(Outcome::Success)
                    } else {
                        Err(index)
                    }
                })
            })
        }
    }

    impl PartialEq for TestJob {
        fn eq(&self, other: &Self) -> bool {
            self.index == other.index
        }
    }

    async fn trace(graph: TestGraph) -> (Vec<Option<usize>>, Option<Error<usize>>) {
        let graph = Rc::new(graph);
        let trace = Rc::new(Mutex::new(vec![]));
        let job = TestJob {
            index: 0,
            graph: graph.clone(),
            trace: trace.clone(),
            success: graph[0].0,
        };

        let sched = Scheduler::default();
        let err = sched.run(job).await.err();

        let mut results = vec![None; graph.len()];

        for (finished_idx, job_idx) in trace.lock().await.iter().enumerate() {
            // Ensure no job has had its update method called more than once
            assert!(results[*job_idx].is_none());

            results[*job_idx] = Some(finished_idx);
        }

        (results, err)
    }

    #[async_std::test]
    async fn single_job() {
        let (trace, err) = trace(vec![(true, vec![])]).await;

        assert!(err.is_none());
        assert_eq!(trace[0], Some(0));
    }

    #[async_std::test]
    async fn single_job_fails() {
        let (trace, err) = trace(vec![(false, vec![])]).await;

        assert_eq!(err, Some(Error::Failed(vec![0])));
        assert_eq!(trace[0], Some(0));
    }

    #[async_std::test]
    async fn single_dep() {
        let (trace, err) = trace(vec![(true, vec![1]), (true, vec![])]).await;

        assert!(err.is_none());
        assert_eq!(trace[0], Some(1));
        assert_eq!(trace[1], Some(0));
    }

    #[async_std::test]
    async fn single_dep_fails() {
        let (trace, err) = trace(vec![(true, vec![1]), (false, vec![])]).await;

        assert_eq!(err, Some(Error::Failed(vec![1])));
        assert_eq!(trace[0], None);
        assert_eq!(trace[1], Some(0));
    }

    #[async_std::test]
    async fn single_dep_root_fails() {
        let (trace, err) = trace(vec![(false, vec![1]), (true, vec![])]).await;

        assert_eq!(err, Some(Error::Failed(vec![0])));
        assert_eq!(trace[0], Some(1));
        assert_eq!(trace[1], Some(0));
    }

    #[async_std::test]
    async fn two_deps() {
        let (trace, err) = trace(vec![(true, vec![1, 2]), (true, vec![]), (true, vec![])]).await;

        assert!(err.is_none());
        assert_eq!(trace[0], Some(2));
        assert!(matches!(trace[1], Some(x) if x < 2));
        assert!(matches!(trace[2], Some(x) if x < 2));
    }

    #[async_std::test]
    async fn two_deps_one_fails() {
        let (trace, err) = trace(vec![(true, vec![1, 2]), (true, vec![]), (false, vec![])]).await;

        assert_eq!(err, Some(Error::Failed(vec![2])));
        assert_eq!(trace[0], None);
        // job 1 may or may not be updated
        assert!(trace[2].is_some());
    }

    #[async_std::test]
    async fn single_trans_dep() {
        let (trace, err) = trace(vec![(true, vec![1]), (true, vec![2]), (true, vec![])]).await;

        assert!(err.is_none());
        assert_eq!(trace[0], Some(2));
        assert_eq!(trace[1], Some(1));
        assert_eq!(trace[2], Some(0));
    }

    #[async_std::test]
    async fn single_trans_dep_fails() {
        let (trace, err) = trace(vec![(true, vec![1]), (true, vec![2]), (false, vec![])]).await;

        assert_eq!(err, Some(Error::Failed(vec![2])));
        assert_eq!(trace[0], None);
        assert_eq!(trace[1], None);
        assert_eq!(trace[2], Some(0));
    }

    #[async_std::test]
    async fn single_trans_dep_direct_dep_fails() {
        let (trace, err) = trace(vec![(true, vec![1]), (false, vec![2]), (true, vec![])]).await;

        assert_eq!(err, Some(Error::Failed(vec![1])));
        assert_eq!(trace[0], None);
        assert_eq!(trace[1], Some(1));
        assert_eq!(trace[2], Some(0));
    }

    #[async_std::test]
    async fn two_deps_single_trans_dep() {
        let (trace, err) = trace(vec![
            (true, vec![1, 3]),
            (true, vec![2]),
            (true, vec![]),
            (true, vec![]),
        ])
        .await;

        assert!(err.is_none());
        assert_eq!(trace[0], Some(3));
        assert!(matches!(trace[3], Some(x) if x < 3));

        let order_of_1 = trace[1].unwrap();
        let order_of_2 = trace[2].unwrap();
        assert!(order_of_1 > order_of_2);
        assert!(order_of_1 < 3);
    }

    #[async_std::test]
    async fn two_deps_each_with_trans_dep() {
        let (trace, err) = trace(vec![
            (true, vec![1, 3]),
            (true, vec![2]),
            (true, vec![]),
            (true, vec![4]),
            (true, vec![]),
        ])
        .await;

        assert!(err.is_none());
        assert_eq!(trace[0], Some(4));

        let order_of_1 = trace[1].unwrap();
        let order_of_2 = trace[2].unwrap();
        assert!(order_of_1 < 4);
        assert!(order_of_2 < 4);
        assert!(order_of_1 > order_of_2);

        let order_of_3 = trace[3].unwrap();
        let order_of_4 = trace[4].unwrap();
        assert!(order_of_3 < 4);
        assert!(order_of_4 < 4);
        assert!(order_of_3 > order_of_4);
    }

    #[async_std::test]
    async fn three_deps() {
        let (trace, err) = trace(vec![
            (true, vec![1, 2, 3]),
            (true, vec![]),
            (true, vec![]),
            (true, vec![]),
        ])
        .await;

        assert!(err.is_none());
        assert_eq!(trace[0], Some(3));
        assert!(matches!(trace[1], Some(x) if x < 3));
        assert!(matches!(trace[2], Some(x) if x < 3));
        assert!(matches!(trace[3], Some(x) if x < 3));
    }

    #[async_std::test]
    async fn diamond() {
        let (trace, err) = trace(vec![
            (true, vec![2, 3]),
            (true, vec![]),
            (true, vec![1]),
            (true, vec![1]),
        ])
        .await;

        assert!(err.is_none());
        assert_eq!(trace[0], Some(3));
        assert_eq!(trace[1], Some(0));

        let order_of_2 = trace[2].unwrap();
        let order_of_3 = trace[3].unwrap();
        assert!(order_of_2 > 0);
        assert!(order_of_2 < 3);
        assert!(order_of_3 > 0);
        assert!(order_of_3 < 3);
    }

    #[async_std::test]
    async fn diamond_with_extra_trans_deps() {
        let (trace, err) = trace(vec![
            (true, vec![2, 3]),
            (true, vec![4]),
            (true, vec![1, 5]),
            (true, vec![1, 6]),
            (true, vec![]),
            (true, vec![]),
            (true, vec![]),
        ])
        .await;

        assert!(err.is_none());
        assert_eq!(trace[0], Some(6));

        let order_of_2 = trace[2].unwrap();
        assert!(order_of_2 < 6);

        let order_of_3 = trace[3].unwrap();
        assert!(order_of_3 < 6);

        let order_of_1 = trace[1].unwrap();
        assert!(order_of_1 < order_of_2);
        assert!(order_of_1 < order_of_3);

        let order_of_4 = trace[4].unwrap();
        assert!(order_of_4 < order_of_1);

        let order_of_5 = trace[5].unwrap();
        assert!(order_of_5 < order_of_2);

        let order_of_6 = trace[6].unwrap();
        assert!(order_of_6 < order_of_3);
    }

    #[async_std::test]
    async fn simple_cycle() {
        let (trace, err) = trace(vec![(true, vec![1]), (true, vec![0])]).await;

        assert_eq!(err, Some(Error::Cycle));
        for job in trace {
            assert_eq!(job, None);
        }
    }

    #[async_std::test]
    async fn complex_cycle() {
        let (trace, err) = trace(vec![
            (true, vec![1, 2]),
            (true, vec![3]),
            (true, vec![1]),
            (true, vec![2]),
        ])
        .await;

        assert_eq!(err, Some(Error::Cycle));
        for job in trace {
            assert_eq!(job, None);
        }
    }
}
