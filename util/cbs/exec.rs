use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex};

use sha2::Digest;

use crate::core::*;

#[derive(Debug)]
pub struct Executor {
    context: Context,
    tasks: Mutex<TaskGraph>,
    task_events: Condvar,

    resolvers: Vec<Box<dyn ResolverPlugin>>,
    dependency_planners: Vec<Box<dyn DependencyPlannerPlugin>>,
    builders: Mutex<HashMap<String, Arc<dyn BuildPlugin>>>,
}

#[derive(Debug)]
pub struct TaskGraph {
    tasks: Vec<Task>,
    by_target: HashMap<String, usize>,
    rdeps: Vec<Vec<usize>>,
}

enum WorkItem {
    Task(Task),
    Done,
    Deadlocked(String),
}

impl Executor {
    pub fn new() -> Self {
        let mut context = Context::new(std::path::PathBuf::from("/tmp/cache"), std::iter::empty());
        context.calculate_hash();

        Self {
            context,
            tasks: Mutex::new(TaskGraph::new()),
            task_events: Condvar::new(),

            resolvers: Vec::new(),
            dependency_planners: Vec::new(),
            builders: Mutex::new(HashMap::new()),
        }
    }

    pub fn with_config<T: IntoIterator<Item = (BuildConfigKey, String)>>(config: T) -> Self {
        let mut context = Context::new(std::path::PathBuf::from("/tmp/cache"), config);
        context.calculate_hash();
        Self::with_context(context)
    }

    pub fn with_context(context: Context) -> Self {
        Self {
            context,
            tasks: Mutex::new(TaskGraph::new()),
            task_events: Condvar::new(),

            resolvers: Vec::new(),
            dependency_planners: Vec::new(),
            builders: Mutex::new(HashMap::new()),
        }
    }

    pub fn add_resolver_plugin(&mut self, resolver: Box<dyn ResolverPlugin>) {
        self.resolvers.push(resolver);
    }

    pub fn add_dependency_planner_plugin(&mut self, planner: Box<dyn DependencyPlannerPlugin>) {
        self.dependency_planners.push(planner);
    }

    pub fn add_builder_plugin<T: Into<String>>(
        &mut self,
        target: T,
        builder: Arc<dyn BuildPlugin>,
    ) {
        self.builders.lock().unwrap().insert(target.into(), builder);
    }

    pub fn add_task<T: Into<String>>(&self, target: T, rdep: Option<usize>) -> usize {
        let target: String = target.into();
        let is_builder = self.builders.lock().unwrap().contains_key(&target);
        let mut graph = self.tasks.lock().unwrap();
        let exists = graph.by_target.contains_key(&target);
        let id = graph.add_task(target, rdep);
        if !exists && is_builder {
            graph.mark_build_success(id, BuildResult::noop(), 0);
        }
        id
    }

    pub fn run(&mut self, roots: &[usize]) -> BuildResult {
        eprintln!("[cbs] planning external dependencies");
        if let Err(e) = self.plan_external_dependencies() {
            return BuildResult::Failure(format!("dependency planning failed:\n{e}"));
        }
        if self.all_tasks_done() {
            return self.collect_results(roots);
        }

        let workers = self.worker_count();
        eprintln!("[cbs] building graph with {workers} worker(s)");
        if let Some(error) = self.run_workers(workers) {
            return BuildResult::Failure(error);
        }

        self.collect_results(roots)
    }

    fn all_tasks_done(&self) -> bool {
        self.tasks
            .lock()
            .unwrap()
            .tasks
            .iter()
            .all(|task| task.status() == TaskStatus::Done)
    }

    fn any_tasks_failed(&self) -> bool {
        self.tasks
            .lock()
            .unwrap()
            .tasks
            .iter()
            .any(|task| matches!(task.result.as_ref(), Some(BuildResult::Failure(_))))
    }

    fn collect_results(&self, roots: &[usize]) -> BuildResult {
        let graph = self.tasks.lock().unwrap();
        for task in &graph.tasks {
            if task.status() != TaskStatus::Done {
                return BuildResult::Failure(format!(
                    "not all tasks finished, deadlock! still waiting on {task:?}",
                ));
            }

            match &task.result {
                Some(BuildResult::Success { .. }) => continue,
                Some(BuildResult::Failure(reason)) => {
                    return BuildResult::Failure(reason.to_string());
                }
                None => {
                    return BuildResult::Failure(String::from("not all tasks produced a result"))
                }
            }
        }

        BuildResult::merged(roots.iter().map(|r| {
            graph.tasks[*r]
                .result
                .as_ref()
                .expect("result must be available")
        }))
    }

    fn worker_count(&self) -> usize {
        std::env::var("CBS_JOBS")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .filter(|jobs| *jobs > 0)
            .unwrap_or_else(|| {
                std::thread::available_parallelism()
                    .map(|parallelism| parallelism.get())
                    .unwrap_or(1)
            })
    }

    fn run_workers(&self, workers: usize) -> Option<String> {
        std::thread::scope(|scope| {
            let mut handles = Vec::new();
            for _ in 0..workers {
                handles.push(scope.spawn(|| self.worker_loop()));
            }

            handles.into_iter().find_map(|handle| match handle.join() {
                Ok(None) => None,
                Ok(Some(error)) => Some(error),
                Err(_) => Some("build worker panicked".to_string()),
            })
        })
    }

    fn worker_loop(&self) -> Option<String> {
        loop {
            match self.next_parallel_task() {
                WorkItem::Task(task) => match task.status() {
                    TaskStatus::Resolving => self.resolve(task),
                    TaskStatus::Building => self.build(task),
                    TaskStatus::Blocked | TaskStatus::Done => {
                        unreachable!("cannot acquire a blocked or done task!")
                    }
                },
                WorkItem::Done => return None,
                WorkItem::Deadlocked(reason) => return Some(reason),
            }
        }
    }

    fn next_parallel_task(&self) -> WorkItem {
        let mut graph = self.tasks.lock().unwrap();
        loop {
            for task in &mut graph.tasks {
                if task.available {
                    match task.status() {
                        TaskStatus::Resolving | TaskStatus::Building => {
                            task.available = false;
                            return WorkItem::Task(task.clone());
                        }
                        TaskStatus::Blocked | TaskStatus::Done => continue,
                    }
                }
            }

            if graph
                .tasks
                .iter()
                .all(|task| task.status() == TaskStatus::Done)
            {
                return WorkItem::Done;
            }

            let running = graph.tasks.iter().any(|task| {
                !task.available
                    && matches!(task.status(), TaskStatus::Resolving | TaskStatus::Building)
            });
            if !running {
                let waiting: Vec<_> = graph
                    .tasks
                    .iter()
                    .filter(|task| task.status() != TaskStatus::Done)
                    .map(|task| task.target.clone())
                    .collect();
                self.task_events.notify_all();
                return WorkItem::Deadlocked(format!(
                    "build graph deadlocked; waiting on {}",
                    waiting.join(", ")
                ));
            }

            graph = self.task_events.wait(graph).unwrap();
        }
    }

    fn plan_external_dependencies(&mut self) -> std::io::Result<()> {
        while let Some(task) = self.next_planning_task() {
            self.resolve(task);
        }
        if self.any_tasks_failed() {
            return Ok(());
        }

        let mut requirements: HashMap<String, Vec<ExternalRequirement>> = HashMap::new();
        {
            let graph = self.tasks.lock().unwrap();
            for task in &graph.tasks {
                let Some(config) = task.config.as_ref() else {
                    continue;
                };
                for req in &config.external_requirements {
                    requirements
                        .entry(req.ecosystem.clone())
                        .or_default()
                        .push(req.clone());
                }
            }
        }

        if requirements.is_empty() {
            eprintln!("[cbs] no external dependencies to plan");
            return Ok(());
        }

        let mut lockfile = self.context.lockfile.as_ref().clone();
        let mut locked_dependencies = self.context.locked_dependencies.as_ref().clone();
        for (ecosystem, reqs) in requirements {
            eprintln!("[cbs] plan {ecosystem}: {} root requirement(s)", reqs.len());
            let planner = self
                .dependency_planners
                .iter()
                .find(|planner| planner.ecosystem() == ecosystem)
                .ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        format!("no dependency planner registered for ecosystem {ecosystem}"),
                    )
                })?;
            let plan = planner.plan(self.context.clone(), &reqs)?;
            eprintln!(
                "[cbs] planned {ecosystem}: {} lock entry(s), {} dependency edge set(s)",
                plan.lockfile.len(),
                plan.locked_dependencies.len()
            );
            lockfile.extend(plan.lockfile);
            locked_dependencies.extend(plan.locked_dependencies);
        }

        self.context.lockfile = Arc::new(lockfile);
        self.context.locked_dependencies = Arc::new(locked_dependencies);
        self.context.calculate_hash();
        Ok(())
    }

    fn next_planning_task(&self) -> Option<Task> {
        let mut graph = self.tasks.lock().unwrap();
        for task in &mut graph.tasks {
            if !task.available || task.status() != TaskStatus::Resolving {
                continue;
            }
            if self.should_defer_external_target(&task.target) {
                continue;
            }
            task.available = false;
            return Some(task.clone());
        }
        None
    }

    fn should_defer_external_target(&self, target: &str) -> bool {
        self.context.get_locked_version(target).is_err()
            && self
                .dependency_planners
                .iter()
                .any(|planner| planner.can_plan_target(target))
    }

    pub fn resolve(&self, task: Task) {
        eprintln!("[cbs] resolve {}", task.target);
        for resolver in &self.resolvers {
            if !resolver.can_resolve(&task.target) {
                continue;
            }
            match resolver.resolve(self.context.with_target(&task.target), &task.target) {
                Ok(config) => {
                    // Add all dependent tasks first
                    let deps: Vec<usize> = config
                        .dependencies()
                        .into_iter()
                        .map(|t| self.add_task(t, Some(task.id)))
                        .collect();

                    let mut graph = self.tasks.lock().unwrap();

                    // It's possible that some of the dependencies are already ready, so pre-set
                    // the right ready count.
                    let dependencies_ready = deps
                        .iter()
                        .filter(|id| graph.tasks[**id].status() == TaskStatus::Done)
                        .count();

                    let t = &mut graph.tasks[task.id];
                    t.dependencies = deps;
                    t.dependencies_ready = dependencies_ready;

                    t.config = Some(config);
                    t.available = true;
                    drop(graph);
                    self.task_events.notify_all();
                }
                Err(e) => {
                    self.mark_task_failure(
                        task.id,
                        BuildResult::Failure(format!("target resolution failed:\n{e}")),
                    );
                }
            }
            return;
        }

        // No resolver available for the target!
        self.mark_task_failure(
            task.id,
            BuildResult::Failure(format!("no resolver available for target {}", task.target)),
        );
    }

    pub fn build(&self, mut task: Task) {
        let config = task
            .config
            .as_mut()
            .expect("must have config resolved before build can begin!");

        let plugin = {
            let mut builders = self.builders.lock().unwrap();
            if let Some(p) = builders.get(&config.build_plugin) {
                p.clone()
            } else {
                // Load the plugin from built dependencies
                let graph = self.tasks.lock().unwrap();
                let plugin_task = match graph.by_target.get(&config.build_plugin) {
                    Some(t) => &graph.tasks[*t],
                    None => {
                        panic!("we must have already loaded this plugin's target by now!");
                    }
                };
                let plugin_path = match plugin_task
                    .result
                    .as_ref()
                    .expect("this plugin must already have been built!")
                {
                    BuildResult::Success(BuildOutput { outputs, .. }) => outputs[0].clone(),
                    _ => panic!("the plugin build must have succeeded by now!"),
                };
                let plugin = load_plugin(&plugin_path);
                builders.insert(config.build_plugin.clone(), plugin.clone());
                plugin
            }
        };

        let mut dep_hash = sha2::Sha256::new();
        let mut deps = HashMap::new();
        {
            let graph = self.tasks.lock().unwrap();
            for dep in config.dependencies() {
                let dt = match graph.by_target.get(&dep) {
                    Some(t) => &graph.tasks[*t],
                    None => {
                        panic!("all dependencies must exist by now!");
                    }
                };

                if let Some(cfg) = dt.config.as_ref() {
                    dep_hash.update(cfg.hash.to_be_bytes());
                }

                match dt.result.as_ref() {
                    Some(BuildResult::Success(out)) => {
                        deps.insert(dt.target.clone(), out.clone());
                    }
                    Some(BuildResult::Failure(_)) => {
                        panic!("all dependencies must be succesfully built by now!");
                    }
                    None => panic!("all dependencies must be finished building by now!"),
                }
            }
        }

        let dep_hash = u64::from_be_bytes(
            dep_hash.finalize()[..8]
                .try_into()
                .expect("invalid hash size"),
        );
        let hash = config.calculate_hash(self.context.hash, dep_hash);
        let kind = if config.kind.is_empty() {
            config.build_plugin.clone()
        } else {
            config.kind.clone()
        };

        let _t = task.clone();
        let ctx = self.context.with_task(&_t);
        if let Some(output) = read_cached_build_output(&ctx) {
            eprintln!("[cbs] cache hit {}", task.target);
            self.mark_build_success(task.id, BuildResult::Success(output), hash);
            return;
        }

        eprintln!("[cbs] build {} ({kind})", task.target);
        let result = plugin.build(ctx, _t, deps);
        match result {
            BuildResult::Success(ref output) => {
                if let Err(e) = write_cached_build_output(&self.context.with_task(&task), output) {
                    eprintln!(
                        "[cbs] warning: failed to write cache for {}: {e}",
                        task.target
                    );
                }
                self.mark_build_success(task.id, result, hash);
            }
            BuildResult::Failure(_) => {
                self.mark_task_failure(task.id, result);
            }
        }
    }

    pub fn mark_task_failure(&self, id: usize, result: BuildResult) {
        {
            let graph = self.tasks.lock().unwrap();
            let task = graph.tasks.get(id).unwrap();
            let stage = match task.failure_stage() {
                TaskStatus::Resolving => "resolve",
                TaskStatus::Building => "build",
                _ => "??",
            };
            eprintln!("\n[cbs] error: {stage} failed");
            eprintln!("  target: {}", task.target);
            if let BuildResult::Failure(ref msg) = result {
                eprintln!("  reason:");
                print_indented(msg, 4);
            }
            if let Some(msgs) = self.context.logs.read().unwrap().get(&task.target) {
                eprintln!("  logs:");
                for msg in msgs.lock().unwrap().iter() {
                    print_indented(msg, 4);
                }
            }
        }

        self.tasks.lock().unwrap().mark_task_failure(id, id, result);
        self.task_events.notify_all();
    }

    pub fn print_all_logs(&self) {
        for (target, logs) in self.context.logs.read().unwrap().iter() {
            println!("\nlogs from {}:\n", target);
            for msg in logs.lock().unwrap().iter() {
                println!("{}", msg);
            }
        }
    }

    pub fn mark_build_success(&self, id: usize, result: BuildResult, hash: u64) {
        self.tasks
            .lock()
            .unwrap()
            .mark_build_success(id, result, hash);
        self.task_events.notify_all();
    }
}

fn print_indented(message: &str, spaces: usize) {
    let indent = " ".repeat(spaces);
    for line in message.lines() {
        eprintln!("{indent}{line}");
    }
}

fn read_cached_build_output(context: &Context) -> Option<BuildOutput> {
    let path = build_output_cache_file(context);
    let content = std::fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&content).ok()?;
    let output = parse_cached_build_output(&value).ok()?;
    if build_output_paths_exist(&output) {
        Some(output)
    } else {
        None
    }
}

fn write_cached_build_output(context: &Context, output: &BuildOutput) -> std::io::Result<()> {
    let path = build_output_cache_file(context);
    let tmp = path.with_extension("json.tmp");
    std::fs::create_dir_all(path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("cache path has no parent: {}", path.display()),
        )
    })?)?;
    std::fs::write(
        &tmp,
        serde_json::json!({
            "outputs": output.outputs.iter().map(|path| path.display().to_string()).collect::<Vec<_>>(),
            "extras": output.extras.iter().map(|(key, values)| (key.to_string(), values.clone())).collect::<HashMap<_, _>>(),
        })
        .to_string(),
    )?;
    std::fs::rename(tmp, path)
}

fn build_output_cache_file(context: &Context) -> std::path::PathBuf {
    context.working_directory().join("build-output.json")
}

fn parse_cached_build_output(value: &serde_json::Value) -> std::io::Result<BuildOutput> {
    let outputs = value
        .get("outputs")
        .and_then(|outputs| outputs.as_array())
        .ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "cache missing outputs")
        })?
        .iter()
        .map(|output| {
            output
                .as_str()
                .map(std::path::PathBuf::from)
                .ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "cached output path must be a string",
                    )
                })
        })
        .collect::<std::io::Result<Vec<_>>>()?;
    let extras = value
        .get("extras")
        .and_then(|extras| extras.as_object())
        .map(|extras| {
            extras
                .iter()
                .map(|(key, values)| {
                    let key = key.parse::<u32>().map_err(|e| {
                        std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            format!("cached extra key must be a u32: {e}"),
                        )
                    })?;
                    let values = values
                        .as_array()
                        .ok_or_else(|| {
                            std::io::Error::new(
                                std::io::ErrorKind::InvalidData,
                                "cached extra values must be an array",
                            )
                        })?
                        .iter()
                        .map(|value| {
                            value
                                .as_str()
                                .map(|value| value.to_string())
                                .ok_or_else(|| {
                                    std::io::Error::new(
                                        std::io::ErrorKind::InvalidData,
                                        "cached extra value must be a string",
                                    )
                                })
                        })
                        .collect::<std::io::Result<Vec<_>>>()?;
                    Ok((key, values))
                })
                .collect::<std::io::Result<HashMap<_, _>>>()
        })
        .transpose()?
        .unwrap_or_default();

    Ok(BuildOutput { outputs, extras })
}

fn build_output_paths_exist(output: &BuildOutput) -> bool {
    output.outputs.iter().all(|path| path.exists())
        && output.extras.values().flatten().all(|value| {
            value
                .split_once(':')
                .map(|(_, path)| std::path::Path::new(path).exists())
                .unwrap_or(true)
        })
}

impl TaskGraph {
    pub fn new() -> Self {
        Self {
            tasks: Vec::new(),
            by_target: HashMap::new(),
            rdeps: Vec::new(),
        }
    }

    pub fn add_task<T: Into<String>>(&mut self, target: T, rdep: Option<usize>) -> usize {
        let target: String = target.into();

        if let Some(id) = self.by_target.get(&target) {
            if let Some(r) = rdep {
                self.rdeps[*id].push(r);
            }

            return *id;
        }

        match rdep {
            Some(r) => self.rdeps.push(vec![r]),
            None => self.rdeps.push(Vec::new()),
        }

        let id = self.tasks.len();
        self.tasks.push(Task::new(id, target.clone()));
        self.by_target.insert(target.into(), id);
        id
    }

    pub fn mark_task_failure(&mut self, id: usize, root_cause: usize, result: BuildResult) {
        self.tasks[id].result = Some(result);
        self.tasks[id].available = true;
        for rdep in self.rdeps[id].clone() {
            self.mark_task_failure(
                rdep,
                root_cause,
                BuildResult::Failure(format!(
                    "failed to build dependency: {}",
                    self.tasks[root_cause].target
                )),
            );
        }
    }

    pub fn mark_build_success(&mut self, id: usize, result: BuildResult, hash: u64) {
        self.tasks[id].result = Some(result);
        self.tasks[id].available = true;
        if let Some(config) = self.tasks[id].config.as_mut() {
            config.hash = hash;
        }
        for rdep in &self.rdeps[id] {
            self.tasks[*rdep].dependencies_ready += 1;
        }
    }
}

fn load_plugin(path: &std::path::Path) -> Arc<dyn BuildPlugin> {
    #[cfg(test)]
    if let Some(plugin) = load_test_builtin_plugin(path) {
        return plugin;
    }

    if path.exists() {
        return match crate::plugin_abi::load_build_plugin(path) {
            Ok(plugin) => Arc::new(plugin),
            Err(e) => Arc::new(PluginLoadFailure {
                message: e.to_string(),
            }),
        };
    }

    Arc::new(PluginLoadFailure {
        message: format!("plugin path {} does not exist", path.display()),
    })
}

#[cfg(test)]
fn load_test_builtin_plugin(path: &std::path::Path) -> Option<Arc<dyn BuildPlugin>> {
    match path.file_name().and_then(|name| name.to_str()) {
        Some("rust.cdylib") => Some(Arc::new(
            crate::plugin_abi::AbiBuildPlugin::new(crate::rust_plugin::cbs_plugin_v1())
                .expect("builtin rust plugin must use the current ABI"),
        )),
        Some("bus.cdylib") => Some(Arc::new(
            crate::plugin_abi::AbiBuildPlugin::new(crate::bus::cbs_plugin_v1())
                .expect("builtin bus plugin must use the current ABI"),
        )),
        _ => None,
    }
}

#[derive(Debug)]
struct PluginLoadFailure {
    message: String,
}

impl BuildPlugin for PluginLoadFailure {
    fn build(
        &self,
        _context: Context,
        _task: Task,
        _dependencies: HashMap<String, BuildOutput>,
    ) -> BuildResult {
        BuildResult::Failure(format!("failed to load build plugin: {}", self.message))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cargo::{CargoDependencyPlanner, CargoResolver};

    use crate::plugins::plugin_kind;

    #[test]
    fn test_execution() {
        let mut e = Executor::new();
        e.builders
            .lock()
            .unwrap()
            .insert("@filesystem".to_string(), Arc::new(FilesystemBuilder {}));
        e.resolvers.push(Box::new(FakeResolver::with_configs(vec![
            (
                "//:lhello",
                Ok(Config {
                    build_plugin: "@rust_plugin".to_string(),
                    sources: vec!["/Users/colinwm/Documents/code/cbs/data/lhello.rs".to_string()],
                    build_dependencies: vec!["@rust_compiler".to_string()],
                    kind: plugin_kind::RUST_LIBRARY.to_string(),
                    ..Default::default()
                }),
            ),
            (
                "//:hello_world",
                Ok(Config {
                    build_plugin: "@rust_plugin".to_string(),
                    sources: vec![
                        "/Users/colinwm/Documents/code/cbs/data/hello_world.rs".to_string()
                    ],
                    dependencies: vec!["//:lhello".to_string()],
                    build_dependencies: vec!["@rust_compiler".to_string()],
                    kind: plugin_kind::RUST_BINARY.to_string(),
                    ..Default::default()
                }),
            ),
            (
                "@rust_plugin",
                Ok(Config {
                    build_plugin: "@filesystem".to_string(),
                    location: Some(
                        "/Users/colinwm/Documents/code/cbs/data/rust.cdylib".to_string(),
                    ),
                    ..Default::default()
                }),
            ),
            (
                "@rust_compiler",
                Ok(Config {
                    build_plugin: "@filesystem".to_string(),
                    location: Some("/Users/colinwm/.cargo/bin/rustc".to_string()),
                    ..Default::default()
                }),
            ),
        ])));

        let id = e.add_task("//:hello_world", None);
        let result = e.run(&[id]);
        assert_eq!(
            result,
            BuildResult::Success(BuildOutput {
                outputs: vec![std::path::PathBuf::from(
                    "/tmp/cache/build/___hello_world-6ebb3cba5d8c8cf3/hello_world"
                )],
                ..Default::default()
            })
        );
    }

    #[test]
    fn test_cargo_build() {
        let mut e = Executor::with_config([
            (BuildConfigKey::TargetFamily, "unix".to_string()),
            (BuildConfigKey::TargetOS, "linux".to_string()),
            (BuildConfigKey::TargetEnv, "gnu".to_string()),
        ]);
        e.builders
            .lock()
            .unwrap()
            .insert("@filesystem".to_string(), Arc::new(FilesystemBuilder {}));

        e.dependency_planners
            .push(Box::new(CargoDependencyPlanner::new()));
        e.resolvers.push(Box::new(CargoResolver::new()));
        e.resolvers.push(Box::new(FakeResolver::with_configs(vec![
            (
                "@rust_compiler",
                Ok(Config {
                    build_plugin: "@filesystem".to_string(),
                    location: Some("/Users/colinwm/.cargo/bin/rustc".to_string()),
                    ..Default::default()
                }),
            ),
            (
                "@rust_plugin",
                Ok(Config {
                    build_plugin: "@filesystem".to_string(),
                    location: Some("/tmp/rust.cdylib".to_string()),
                    ..Default::default()
                }),
            ),
            (
                "//:dice_roll",
                Ok(Config {
                    build_plugin: "@rust_plugin".to_string(),
                    sources: vec!["/Users/colinwm/Documents/code/cbs/data/dice_roll.rs".to_string()],
                    external_requirements: vec![ExternalRequirement {
                        ecosystem: "cargo".to_string(),
                        package: "rand".to_string(),
                        version: "=0.8.5".to_string(),
                        features: vec!["std".to_string(), "std_rng".to_string()],
                        default_features: true,
                        target: None,
                    }],
                    build_dependencies: vec!["@rust_compiler".to_string()],
                    kind: plugin_kind::RUST_BINARY.to_string(),
                    ..Default::default()
                }),
            ),
        ])));

        let id = e.add_task("//:dice_roll", None);
        let result = e.run(&[id]);

        // e.print_all_logs();

        let BuildResult::Success(output) = result else {
            panic!("cargo build failed: {result:?}");
        };
        assert_eq!(
            output.outputs[0].file_name().and_then(|name| name.to_str()),
            Some("dice_roll")
        );
    }

    #[test]
    fn test_cargo_libc_build() {
        let mut e = Executor::with_config([
            (BuildConfigKey::TargetFamily, "unix".to_string()),
            (BuildConfigKey::TargetOS, "linux".to_string()),
            (BuildConfigKey::TargetEnv, "gnu".to_string()),
        ]);
        e.builders
            .lock()
            .unwrap()
            .insert("@filesystem".to_string(), Arc::new(FilesystemBuilder {}));

        e.context.lockfile = Arc::new(
            vec![("cargo://libc".to_string(), "0.2.151".to_string())]
                .into_iter()
                .collect(),
        );

        e.resolvers.push(Box::new(CargoResolver::new()));
        e.resolvers.push(Box::new(FakeResolver::with_configs(vec![
            (
                "@rust_compiler",
                Ok(Config {
                    build_plugin: "@filesystem".to_string(),
                    location: Some("/Users/colinwm/.cargo/bin/rustc".to_string()),
                    ..Default::default()
                }),
            ),
            (
                "@rust_plugin",
                Ok(Config {
                    build_plugin: "@filesystem".to_string(),
                    location: Some("/tmp/rust.cdylib".to_string()),
                    ..Default::default()
                }),
            ),
        ])));

        let id = e.add_task("cargo://libc", None);
        let result = e.run(&[id]);

        let BuildResult::Success(output) = result else {
            panic!("libc build failed");
        };
        assert_eq!(
            output.outputs[0]
                .file_name()
                .and_then(|name| name.to_str())
                .map(|name| name.starts_with("liblibc-")),
            Some(true)
        );
    }

    #[test]
    fn test_cargo_hyper_build() {
        let mut e = Executor::with_config([
            (BuildConfigKey::TargetFamily, "unix".to_string()),
            (BuildConfigKey::TargetOS, "linux".to_string()),
            (BuildConfigKey::TargetEnv, "gnu".to_string()),
        ]);
        e.builders
            .lock()
            .unwrap()
            .insert("@filesystem".to_string(), Arc::new(FilesystemBuilder {}));

        let (resolver, mut lockfile) = CargoResolver::from_cargo_lock("Cargo.lock").unwrap();
        lockfile.extend(
            [
                ("cargo://bytes@0.5.6", "0.5.6,default,std"),
                ("cargo://bytes@1.5.0", "1.5.0,default,std"),
                ("cargo://cfg-if@0.1.10", "0.1.10"),
                ("cargo://cfg-if@1.0.0", "1.0.0"),
                ("cargo://fnv", "1.0.7,default,std"),
                ("cargo://futures-channel", "0.3.30,alloc,default,futures-sink,sink,std"),
                ("cargo://futures-core", "0.3.30,alloc,default,std"),
                ("cargo://futures-io", "0.3.30,std"),
                ("cargo://futures-macro", "0.3.30"),
                ("cargo://futures-sink", "0.3.30,alloc,default,std"),
                ("cargo://futures-task", "0.3.30,alloc,std"),
                (
                    "cargo://futures-util",
                    "0.3.30,alloc,async-await,async-await-macro,channel,futures-channel,futures-io,futures-macro,futures-sink,io,memchr,sink,slab,std",
                ),
                ("cargo://h2", "0.2.7"),
                ("cargo://hashbrown@0.12.3", "0.12.3,raw"),
                ("cargo://http", "0.2.11"),
                ("cargo://http-body", "0.3.1"),
                ("cargo://httparse", "1.8.0,default,std"),
                ("cargo://httpdate", "0.3.2"),
                ("cargo://hyper", "0.13.10,stream"),
                ("cargo://indexmap@1.9.3", "1.9.3,std"),
                ("cargo://iovec", "0.1.4"),
                ("cargo://itoa@0.4.8", "0.4.8,default,std"),
                ("cargo://itoa@1.0.10", "1.0.10"),
                ("cargo://lazy_static", "1.4.0"),
                ("cargo://libc", "0.2.151,align,default,extra_traits,std"),
                ("cargo://log", "0.4.20"),
                ("cargo://memchr", "2.6.4,alloc,default,std"),
                ("cargo://mio", "0.6.23,default,with-deprecated"),
                ("cargo://mio-uds", "0.6.8"),
                ("cargo://net2", "0.2.39,default,duration"),
                ("cargo://num_cpus", "1.16.0"),
                ("cargo://once_cell", "1.19.0,alloc,default,race,std"),
                ("cargo://pin-project", "1.1.3"),
                ("cargo://pin-project-internal", "1.1.3"),
                ("cargo://pin-project-lite@0.1.12", "0.1.12"),
                ("cargo://pin-project-lite@0.2.13", "0.2.13"),
                ("cargo://pin-utils", "0.1.0"),
                ("cargo://proc-macro2", "1.0.106,default,proc-macro"),
                ("cargo://quote", "1.0.45,default,proc-macro"),
                ("cargo://signal-hook-registry", "1.4.1"),
                ("cargo://slab", "0.4.9,default,std"),
                ("cargo://socket2", "0.3.19"),
                (
                    "cargo://syn@1.0.109",
                    "1.0.109,clone-impls,default,derive,full,parsing,printing,proc-macro,quote",
                ),
                (
                    "cargo://syn@2.0.117",
                    "2.0.117,clone-impls,default,derive,full,parsing,printing,proc-macro,quote,visit-mut",
                ),
                (
                    "cargo://tokio",
                    "0.2.25,blocking,default,dns,fnv,fs,full,futures-core,io-driver,io-std,io-util,iovec,lazy_static,libc,macros,memchr,mio,mio-uds,net,num_cpus,rt-core,rt-threaded,rt-util,signal,signal-hook-registry,slab,stream,sync,tcp,time,tokio-macros,udp,uds",
                ),
                ("cargo://tokio-macros", "0.2.6"),
                ("cargo://tokio-util", "0.3.1,codec,default"),
                ("cargo://tower-service", "0.3.2"),
                ("cargo://tracing", "0.1.40,log,std"),
                ("cargo://tracing-core", "0.1.32,once_cell,std"),
                ("cargo://tracing-futures", "0.2.5,pin-project,std-future"),
                ("cargo://try-lock", "0.2.5"),
                ("cargo://unicode-ident", "1.0.12"),
                ("cargo://want", "0.3.1"),
            ]
            .into_iter()
            .map(|(target, lockstring)| (target.to_string(), lockstring.to_string())),
        );
        e.context.lockfile = Arc::new(lockfile);

        e.resolvers.push(Box::new(resolver));
        e.resolvers.push(Box::new(FakeResolver::with_configs(vec![
            (
                "@rust_compiler",
                Ok(Config {
                    build_plugin: "@filesystem".to_string(),
                    location: Some("/Users/colinwm/.cargo/bin/rustc".to_string()),
                    ..Default::default()
                }),
            ),
            (
                "@rust_plugin",
                Ok(Config {
                    build_plugin: "@filesystem".to_string(),
                    location: Some("/tmp/rust.cdylib".to_string()),
                    ..Default::default()
                }),
            ),
            (
                "//:hyper_headers",
                Ok(Config {
                    build_plugin: "@rust_plugin".to_string(),
                    sources: vec![
                        "/Users/colinwm/Documents/code/cbs/data/hyper_headers.rs".to_string()
                    ],
                    dependencies: vec!["cargo://hyper".to_string()],
                    build_dependencies: vec!["@rust_compiler".to_string()],
                    kind: plugin_kind::RUST_BINARY.to_string(),
                    ..Default::default()
                }),
            ),
        ])));

        let id = e.add_task("//:hyper_headers", None);
        let result = e.run(&[id]);

        let BuildResult::Success(output) = result else {
            panic!("hyper build failed");
        };
        assert_eq!(
            output.outputs[0].file_name().and_then(|name| name.to_str()),
            Some("hyper_headers")
        );
    }

    #[test]
    fn test_cargo_serde_derive_build() {
        let mut e = Executor::with_config([
            (BuildConfigKey::TargetFamily, "unix".to_string()),
            (BuildConfigKey::TargetOS, "linux".to_string()),
            (BuildConfigKey::TargetEnv, "gnu".to_string()),
        ]);
        e.builders
            .lock()
            .unwrap()
            .insert("@filesystem".to_string(), Arc::new(FilesystemBuilder {}));

        let resolver = CargoResolver::new().with_locked_dependencies([
            (
                "cargo://proc-macro2",
                vec![("unicode_ident", "cargo://unicode-ident")],
            ),
            (
                "cargo://quote",
                vec![("proc_macro2", "cargo://proc-macro2")],
            ),
            (
                "cargo://serde_derive",
                vec![
                    ("proc_macro2", "cargo://proc-macro2"),
                    ("quote", "cargo://quote"),
                    ("syn", "cargo://syn@2.0.43"),
                ],
            ),
            (
                "cargo://syn@2.0.43",
                vec![
                    ("proc_macro2", "cargo://proc-macro2"),
                    ("quote", "cargo://quote"),
                    ("unicode_ident", "cargo://unicode-ident"),
                ],
            ),
        ]);
        let mut lockfile = HashMap::new();
        lockfile.extend(
            [
                ("cargo://proc-macro2", "1.0.71,default,proc-macro"),
                ("cargo://quote", "1.0.33,default,proc-macro"),
                ("cargo://serde", "1.0.193,default,std"),
                ("cargo://serde_derive", "1.0.193,default"),
                (
                    "cargo://syn@2.0.43",
                    "2.0.43,clone-impls,default,derive,parsing,printing,proc-macro,quote",
                ),
                ("cargo://unicode-ident", "1.0.12"),
            ]
            .into_iter()
            .map(|(target, lockstring)| (target.to_string(), lockstring.to_string())),
        );
        e.context.lockfile = Arc::new(lockfile);

        e.resolvers.push(Box::new(resolver));
        e.resolvers.push(Box::new(FakeResolver::with_configs(vec![
            (
                "@rust_compiler",
                Ok(Config {
                    build_plugin: "@filesystem".to_string(),
                    location: Some("/Users/colinwm/.cargo/bin/rustc".to_string()),
                    ..Default::default()
                }),
            ),
            (
                "@rust_plugin",
                Ok(Config {
                    build_plugin: "@filesystem".to_string(),
                    location: Some("/tmp/rust.cdylib".to_string()),
                    ..Default::default()
                }),
            ),
            (
                "//:serde_derive_smoke",
                Ok(Config {
                    build_plugin: "@rust_plugin".to_string(),
                    sources: vec![
                        "/Users/colinwm/Documents/code/cbs/data/serde_derive_smoke.rs".to_string(),
                    ],
                    dependencies: vec![
                        "cargo://serde".to_string(),
                        "cargo://serde_derive".to_string(),
                    ],
                    build_dependencies: vec!["@rust_compiler".to_string()],
                    kind: plugin_kind::RUST_BINARY.to_string(),
                    ..Default::default()
                }),
            ),
        ])));

        let id = e.add_task("//:serde_derive_smoke", None);
        let result = e.run(&[id]);

        let BuildResult::Success(output) = result else {
            panic!("serde derive build failed");
        };
        assert_eq!(
            output.outputs[0].file_name().and_then(|name| name.to_str()),
            Some("serde_derive_smoke")
        );
    }

    #[test]
    fn test_cargo_rustls_build() {
        let mut e = Executor::with_config([
            (BuildConfigKey::TargetFamily, "unix".to_string()),
            (BuildConfigKey::TargetOS, "macos".to_string()),
            (BuildConfigKey::TargetEnv, "".to_string()),
            (BuildConfigKey::TargetArch, "aarch64".to_string()),
            (BuildConfigKey::TargetVendor, "apple".to_string()),
            (BuildConfigKey::TargetEndian, "little".to_string()),
        ]);
        e.builders
            .lock()
            .unwrap()
            .insert("@filesystem".to_string(), Arc::new(FilesystemBuilder {}));

        let resolver = CargoResolver::new();
        e.context.lockfile = Arc::new(
            [
                ("cargo://cfg-if", "1.0.4"),
                ("cargo://getrandom", "0.2.17"),
                ("cargo://libc", "0.2.186"),
                ("cargo://once_cell", "1.21.4,alloc,race,std"),
                ("cargo://ring", "0.17.14,alloc,default,dev_urandom_fallback"),
                ("cargo://rustls", "0.23.31,ring,std"),
                ("cargo://rustls-pki-types", "1.14.1,alloc,default,std"),
                ("cargo://rustls-webpki", "0.103.13,alloc,ring,std"),
                ("cargo://subtle", "2.6.1"),
                ("cargo://untrusted", "0.9.0"),
                ("cargo://zeroize", "1.8.2,alloc,default"),
            ]
            .into_iter()
            .map(|(target, lockstring)| (target.to_string(), lockstring.to_string()))
            .collect(),
        );

        e.resolvers.push(Box::new(resolver));
        e.resolvers.push(Box::new(FakeResolver::with_configs(vec![
            (
                "@rust_compiler",
                Ok(Config {
                    build_plugin: "@filesystem".to_string(),
                    location: Some("/Users/colinwm/.cargo/bin/rustc".to_string()),
                    ..Default::default()
                }),
            ),
            (
                "@rust_plugin",
                Ok(Config {
                    build_plugin: "@filesystem".to_string(),
                    location: Some("/tmp/rust.cdylib".to_string()),
                    ..Default::default()
                }),
            ),
            (
                "//:rustls_smoke",
                Ok(Config {
                    build_plugin: "@rust_plugin".to_string(),
                    sources: vec![
                        "/Users/colinwm/Documents/code/cbs/data/rustls_smoke.rs".to_string()
                    ],
                    dependencies: vec!["cargo://rustls".to_string()],
                    build_dependencies: vec!["@rust_compiler".to_string()],
                    kind: plugin_kind::RUST_BINARY.to_string(),
                    ..Default::default()
                }),
            ),
        ])));

        let id = e.add_task("//:rustls_smoke", None);
        let result = e.run(&[id]);

        let BuildResult::Success(output) = result else {
            panic!("rustls build failed");
        };
        assert_eq!(
            output.outputs[0].file_name().and_then(|name| name.to_str()),
            Some("rustls_smoke")
        );
    }

    #[test]
    fn test_cargo_tokio_runtime_build() {
        let mut e = Executor::with_config([
            (BuildConfigKey::TargetFamily, "unix".to_string()),
            (BuildConfigKey::TargetOS, "linux".to_string()),
            (BuildConfigKey::TargetEnv, "gnu".to_string()),
        ]);
        e.builders
            .lock()
            .unwrap()
            .insert("@filesystem".to_string(), Arc::new(FilesystemBuilder {}));

        let resolver = CargoResolver::new();
        e.context.lockfile = Arc::new(
            [
                ("cargo://bytes", "0.5.6,default,std"),
                ("cargo://fnv", "1.0.7,default,std"),
                ("cargo://hermit-abi", "0.5.2,default"),
                ("cargo://libc", "0.2.186,default,std"),
                ("cargo://num_cpus", "1.17.0"),
                ("cargo://pin-project-lite", "0.1.12"),
                ("cargo://proc-macro2", "1.0.106,default,proc-macro"),
                ("cargo://quote", "1.0.45,default,proc-macro"),
                ("cargo://slab", "0.4.12,default,std"),
                (
                    "cargo://syn",
                    "1.0.109,clone-impls,default,derive,full,parsing,printing,proc-macro,quote",
                ),
                (
                    "cargo://tokio",
                    "0.2.25,default,fnv,macros,num_cpus,rt-core,rt-threaded,slab,sync,time,tokio-macros",
                ),
                ("cargo://tokio-macros", "0.2.6"),
                ("cargo://unicode-ident", "1.0.24"),
            ]
            .into_iter()
            .map(|(target, lockstring)| (target.to_string(), lockstring.to_string()))
            .collect(),
        );

        e.resolvers.push(Box::new(resolver));
        e.resolvers.push(Box::new(FakeResolver::with_configs(vec![
            (
                "@rust_compiler",
                Ok(Config {
                    build_plugin: "@filesystem".to_string(),
                    location: Some("/Users/colinwm/.cargo/bin/rustc".to_string()),
                    ..Default::default()
                }),
            ),
            (
                "@rust_plugin",
                Ok(Config {
                    build_plugin: "@filesystem".to_string(),
                    location: Some("/tmp/rust.cdylib".to_string()),
                    ..Default::default()
                }),
            ),
            (
                "//:tokio_runtime",
                Ok(Config {
                    build_plugin: "@rust_plugin".to_string(),
                    sources: vec![
                        "/Users/colinwm/Documents/code/cbs/data/tokio_runtime.rs".to_string()
                    ],
                    dependencies: vec!["cargo://tokio".to_string()],
                    build_dependencies: vec!["@rust_compiler".to_string()],
                    kind: plugin_kind::RUST_BINARY.to_string(),
                    ..Default::default()
                }),
            ),
        ])));

        let id = e.add_task("//:tokio_runtime", None);
        let result = e.run(&[id]);

        let BuildResult::Success(output) = result else {
            panic!("tokio runtime build failed");
        };
        assert_eq!(
            output.outputs[0].file_name().and_then(|name| name.to_str()),
            Some("tokio_runtime")
        );
    }
}
