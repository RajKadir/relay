/*
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use crate::build_project::{build_project, build_schema, check_project, commit_project};
use crate::compiler_state::{CompilerState, ProjectName};
use crate::config::Config;
use crate::errors::{BuildProjectError, Error, Result};
use crate::graphql_asts::GraphQLAsts;
use crate::{
    artifact_map::ArtifactMap,
    watchman::{source_for_location, FileSource},
};
use common::{PerfLogEvent, PerfLogger};
use graphql_ir::ValidationError;
use log::{error, info};
use rayon::prelude::*;
use schema::Schema;
use std::fmt::Write;
use std::{collections::HashMap, sync::Arc};
use tokio::task;

pub struct Compiler<TPerfLogger>
where
    TPerfLogger: PerfLogger + 'static,
{
    config: Arc<Config>,
    perf_logger: Arc<TPerfLogger>,
}

impl<TPerfLogger: PerfLogger> Compiler<TPerfLogger> {
    pub fn new(config: Config, perf_logger: Arc<TPerfLogger>) -> Self {
        Self {
            config: Arc::new(config),
            perf_logger,
        }
    }

    pub async fn compile(&self) -> Result<CompilerState> {
        let setup_event = self.perf_logger.create_event("compiler_setup");

        let file_source = FileSource::connect(&self.config, &setup_event).await?;
        let mut compiler_state = file_source
            .query(&setup_event, self.perf_logger.as_ref())
            .await?;
        self.build_projects(&mut compiler_state, &setup_event)
            .await?;

        self.perf_logger.complete_event(setup_event);

        Ok(compiler_state)
    }

    pub fn build_schemas(
        &self,
        compiler_state: &CompilerState,
        setup_event: &impl PerfLogEvent,
    ) -> HashMap<ProjectName, Arc<Schema>> {
        let timer = setup_event.start("build_schemas");
        let mut schemas = HashMap::new();
        self.config.for_each_project(|project_config| {
            let schema = build_schema(compiler_state, project_config);
            schemas.insert(project_config.name, Arc::new(schema));
        });
        setup_event.stop(timer);
        schemas
    }

    pub async fn watch_with_callback<F>(&self, mut callback: F) -> Result<()>
    where
        F: FnMut(Result<()>),
    {
        let setup_event = self.perf_logger.create_event("compiler_setup");

        let file_source = FileSource::connect(&self.config, &setup_event).await?;
        let (mut compiler_state, mut subscription) = file_source
            .subscribe(&setup_event, self.perf_logger.as_ref())
            .await?;
        let schemas = self.build_schemas(&compiler_state, &setup_event);
        callback(
            self.check_projects(&mut compiler_state, &schemas, &setup_event)
                .await,
        );

        self.perf_logger.complete_event(setup_event);

        loop {
            if let Some(file_source_changes) = subscription.next_change().await? {
                let incremental_check_event =
                    self.perf_logger.create_event("incremental_check_event");
                let incremental_check_time =
                    incremental_check_event.start("incremental_check_time");

                // TODO Single change to file in VSCode sometimes produces
                // 2 watchman change events for the same file
                let had_new_changes = compiler_state.merge_file_source_changes(
                    &self.config,
                    &file_source_changes,
                    &incremental_check_event,
                    self.perf_logger.as_ref(),
                )?;
                if had_new_changes {
                    // Clear out existing errors
                    callback(Ok(()));
                    // Report any new errors
                    callback(
                        self.check_projects(
                            &mut compiler_state,
                            &schemas,
                            &incremental_check_event,
                        )
                        .await,
                    );
                } else {
                    info!("[watch-mode] No re-compilation required");
                }
                incremental_check_event.stop(incremental_check_time);
                self.perf_logger.complete_event(incremental_check_event);
                // We probably don't want the messages queue to grow indefinitely
                // and we need to flush then, as the check/build is completed
                self.perf_logger.flush();
            }
        }
    }

    pub async fn watch(&self) -> Result<()> {
        let setup_event = self.perf_logger.create_event("compiler_setup");

        let file_source = FileSource::connect(&self.config, &setup_event).await?;

        let (mut compiler_state, mut subscription) = file_source
            .subscribe(&setup_event, self.perf_logger.as_ref())
            .await?;

        if let Err(errors) = self.build_projects(&mut compiler_state, &setup_event).await {
            // TODO correctly print errors
            error!("Errors: {:#?}", errors)
        }
        self.perf_logger.complete_event(setup_event);

        loop {
            if let Some(file_source_changes) = subscription.next_change().await? {
                let incremental_build_event =
                    self.perf_logger.create_event("incremental_build_event");
                let incremental_build_time =
                    incremental_build_event.start("incremental_build_time");

                // TODO Single change to file in VSCode sometimes produces
                // 2 watchman change events for the same file

                info!("\n\n[watch-mode] Change detected");
                let had_new_changes = compiler_state.merge_file_source_changes(
                    &self.config,
                    &file_source_changes,
                    &incremental_build_event,
                    self.perf_logger.as_ref(),
                )?;

                if had_new_changes {
                    if let Err(errors) = self
                        .build_projects(&mut compiler_state, &incremental_build_event)
                        .await
                    {
                        // TODO correctly print errors
                        error!("Errors: {:#?}", errors)
                    }
                } else {
                    info!("[watch-mode] No re-compilation required");
                }
                incremental_build_event.stop(incremental_build_time);
                self.perf_logger.complete_event(incremental_build_event);
                // We probably don't want the messages queue to grow indefinitely
                // and we need to flush then, as the check/build is completed
                self.perf_logger.flush();
            }
        }
    }

    async fn check_projects(
        &self,
        compiler_state: &mut CompilerState,
        schemas: &HashMap<ProjectName, Arc<Schema>>,
        setup_event: &impl PerfLogEvent,
    ) -> Result<()> {
        let graphql_asts = setup_event.time("parse_sources_time", || {
            compiler_state
                .graphql_sources
                .iter()
                .map(|(&source_set_name, sources)| {
                    let asts = GraphQLAsts::from_graphql_sources(sources)?;
                    Ok((source_set_name, asts))
                })
                .collect::<Result<_>>()
        })?;

        let mut build_project_errors = vec![];

        match self.config.only_project {
            Some(project_key) => {
                let project_config =
                    self.config.projects.get(&project_key).unwrap_or_else(|| {
                        panic!("Expected the project {} to exist", &project_key)
                    });
                let schema = Arc::clone(schemas.get(&project_config.name).unwrap());
                check_project(
                    project_config,
                    compiler_state,
                    &graphql_asts,
                    schema,
                    Arc::clone(&self.perf_logger),
                )
                .map_err(|err| {
                    build_project_errors.push(err);
                })
                .ok();
            }
            None => {
                for project_config in self.config.projects.values() {
                    if compiler_state.project_has_pending_changes(project_config.name) {
                        let schema = Arc::clone(schemas.get(&project_config.name).unwrap());
                        // TODO: consider running all projects in parallel
                        check_project(
                            project_config,
                            compiler_state,
                            &graphql_asts,
                            schema,
                            Arc::clone(&self.perf_logger),
                        )
                        .map_err(|err| {
                            build_project_errors.push(err);
                        })
                        .ok();
                    }
                }
            }
        }

        if build_project_errors.is_empty() {
            Ok(())
        } else {
            Err(Error::BuildProjectsErrors {
                errors: build_project_errors,
            })
        }
    }

    async fn build_projects(
        &self,
        compiler_state: &mut CompilerState,
        setup_event: &impl PerfLogEvent,
    ) -> Result<()> {
        let result = build_projects(
            Arc::clone(&self.config),
            Arc::clone(&self.perf_logger),
            setup_event,
            &compiler_state,
        )
        .await;
        match result {
            Ok(next_artifacts) => {
                compiler_state.complete_compilation(next_artifacts);
                Ok(())
            }
            Err(error) => {
                if let Error::BuildProjectsErrors { errors } = &error {
                    for error in errors {
                        self.print_project_error(error);
                    }
                }
                Err(error)
            }
        }
    }

    fn print_project_error(&self, error: &BuildProjectError) {
        if let BuildProjectError::ValidationErrors { errors } = error {
            for ValidationError { message, locations } in errors {
                let locations_and_source: Vec<_> = locations
                    .iter()
                    .map(|&location| {
                        let source = source_for_location(&self.config.root_dir, location);
                        (location, source)
                    })
                    .collect();
                let mut error_message = format!("{}", message);
                for (location, source) in locations_and_source {
                    if let Some(source) = source {
                        write!(
                            error_message,
                            "\n{}",
                            location.print(&source.text, source.line_index, source.column_index)
                        )
                        .unwrap();
                    } else {
                        write!(error_message, "\n{:?}", location).unwrap();
                    }
                }
                error!("{}", error_message);
            }
        };
    }
}

async fn build_projects<TPerfLogger: PerfLogger + 'static>(
    config: Arc<Config>,
    perf_logger: Arc<TPerfLogger>,
    setup_event: &impl PerfLogEvent,
    compiler_state: &CompilerState,
) -> Result<ArtifactMap> {
    let graphql_asts = setup_event.time("parse_sources_time", || {
        GraphQLAsts::from_graphql_sources_map(&compiler_state.graphql_sources)
    })?;

    let build_results: Vec<_> = if let Some(only_project) = config.only_project {
        let project_config = config
            .projects
            .get(&only_project)
            .unwrap_or_else(|| panic!("Expected the project {} to exist", &only_project));
        vec![build_project(
            project_config,
            compiler_state,
            &graphql_asts,
            Arc::clone(&perf_logger),
        )]
    } else {
        config
            .projects
            .par_iter()
            .filter_map(|(_name, project_config)| {
                if compiler_state.project_has_pending_changes(project_config.name) {
                    Some(build_project(
                        project_config,
                        compiler_state,
                        &graphql_asts,
                        Arc::clone(&perf_logger),
                    ))
                } else {
                    None
                }
            })
            .collect()
    };
    let mut results = Vec::new();
    let mut errors = Vec::new();
    for result in build_results {
        match result {
            Ok(result) => results.push(result),
            Err(error) => errors.push(error),
        }
    }

    let errors = if errors.is_empty() {
        let mut handles = Vec::new();
        let errors_mutex = Arc::new(std::sync::Mutex::new(errors));
        for result in results {
            let config = Arc::clone(&config);
            let errors_mutex = Arc::clone(&errors_mutex);
            let perf_logger = Arc::clone(&perf_logger);
            handles.push(task::spawn(async move {
                let (project_name, schema, programs, artifacts) = result;
                let project_config = config
                    .projects
                    .get(&project_name)
                    .unwrap_or_else(|| panic!("Expected the project {} to exist", project_name));
                let result = commit_project(
                    &config,
                    project_config,
                    perf_logger,
                    &schema,
                    programs,
                    artifacts,
                )
                .await;
                match result {
                    Ok(_) => {}
                    Err(error) => {
                        let mut errors = errors_mutex.lock().unwrap();
                        errors.push(error);
                    }
                }
            }))
        }
        futures::future::join_all(handles).await;
        Arc::try_unwrap(errors_mutex).unwrap().into_inner().unwrap()
    } else {
        errors
    };

    if errors.is_empty() {
        let next_artifacts: ArtifactMap = Default::default();
        Ok(next_artifacts)
    } else {
        Err(Error::BuildProjectsErrors { errors })
    }
}
