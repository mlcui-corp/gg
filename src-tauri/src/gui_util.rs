//! Analogous to cli_util from jj-cli
//! We reuse a bit of jj-cli code, but many of its modules include TUI concerns or are not suitable for a long-running server

use std::{cell::OnceCell, collections::HashMap, path::Path, rc::Rc, sync::Arc};

use anyhow::{anyhow, Context, Result};
use itertools::Itertools;
use jj_cli::config::{default_config, LayeredConfigs};
use jj_lib::repo::RepoLoaderError;
use jj_lib::{
    backend::{ChangeId, CommitId},
    commit::Commit,
    hex_util::to_reverse_hex,
    id_prefix::IdPrefixContext,
    object_id::ObjectId,
    op_heads_store,
    operation::Operation,
    repo::{ReadonlyRepo, StoreFactories},
    revset::{
        self, DefaultSymbolResolver, Revset, RevsetAliasesMap, RevsetExpression,
        RevsetParseContext, RevsetWorkspaceContext,
    },
    settings::{ConfigResultExt, UserSettings},
    workspace::{self, Workspace, WorkspaceLoader},
};

use crate::messages;

/// state that doesn't depend on jj-lib borrowings
#[derive(Default)]
pub struct WorkerSession {
    pub latest_query: Option<String>,
}

/// jj-dependent state, available when a workspace is open
pub struct WorkspaceSession<'a> {
    pub session: &'a mut WorkerSession,
    // workspace-level data, initialised once
    pub settings: UserSettings,
    pub workspace: Workspace,
    pub aliases_map: RevsetAliasesMap,
    pub prefix_context: IdPrefixContext,
    //pub parse_context: RevsetParseContext<'b>,
    // operation-specific data, containing a repo view and derived extras
    pub operation: SessionOperation,
}

/// state specific to an operation which may be swapped out
pub struct SessionOperation {
    pub repo: Arc<ReadonlyRepo>,
    pub wc_id: CommitId,
    branches_index: OnceCell<Rc<RefNamesIndex>>,
}

impl WorkerSession {
    pub fn load_directory(&mut self, cwd: &Path) -> Result<WorkspaceSession> {
        let loader = WorkspaceLoader::init(find_workspace_dir(cwd))?;

        let mut configs = LayeredConfigs::from_environment(default_config());
        configs.read_user_config()?;
        configs.read_repo_config(loader.repo_path())?;
        let config = configs.merge();
        let settings = UserSettings::from_config(config);

        let workspace = loader.load(
            &settings,
            &StoreFactories::default(),
            &workspace::default_working_copy_factories(),
        )?;

        let aliases_map = load_revset_aliases(&configs)?;

        let mut prefix_context: IdPrefixContext = IdPrefixContext::default();
        let revset_string: String = settings
            .config()
            .get_string("revsets.short-prefixes")
            .unwrap_or_else(|_| settings.default_revset());
        if !revset_string.is_empty() {
            let disambiguation_revset: Rc<RevsetExpression> = parse_revset(
                &parse_context(&settings, &workspace, &aliases_map),
                &revset_string,
            )?;
            prefix_context = prefix_context.disambiguate_within(disambiguation_revset);
        };

        let operation = WorkspaceSession::load_at_head(&settings, &workspace)?;

        Ok(WorkspaceSession {
            session: self,
            settings,
            workspace,
            aliases_map,
            prefix_context,
            operation,
        })
    }
}

impl WorkspaceSession<'_> {
    fn load_at_head(settings: &UserSettings, workspace: &Workspace) -> Result<SessionOperation> {
        let loader = workspace.repo_loader();

        let op = op_heads_store::resolve_op_heads(
            loader.op_heads_store().as_ref(),
            loader.op_store(),
            |op_heads| {
                let base_repo = loader.load_at(&op_heads[0])?;
                // might want to set some tags
                let mut tx = base_repo.start_transaction(settings);
                for other_op_head in op_heads.into_iter().skip(1) {
                    tx.merge_operation(other_op_head)?;
                    let _num_rebased = tx.mut_repo().rebase_descendants(settings)?;
                }
                Ok::<Operation, RepoLoaderError>(
                    tx.write("resolve concurrent operations")
                        .leave_unpublished()
                        .operation()
                        .clone(),
                )
            },
        )?;

        let repo: Arc<ReadonlyRepo> = workspace
            .repo_loader()
            .load_at(&op)
            .context("load op head")?;

        let wc_id = repo
            .view()
            .get_wc_commit_id(workspace.workspace_id())
            .ok_or_else(|| anyhow!("No working copy found for workspace"))?
            .clone();

        Ok(SessionOperation {
            repo,
            wc_id,
            branches_index: Default::default(),
        })
    }

    /**********************
     * Query/mutation API *
     **********************/

    // XXX creates a parse context and a symbol resolver every time - they need to borrow many things
    pub fn evaluate_revset<'op>(&'op self, revset_str: &str) -> Result<Box<dyn Revset + 'op>> {
        let expression = parse_revset(&self.parse_context(), revset_str)?;
        let resolved_expression =
            expression.resolve_user_expression(self.operation.repo.as_ref(), &self.resolver())?;
        let revset = resolved_expression.evaluate(self.operation.repo.as_ref())?;

        Ok(revset)
    }

    /*************************************************************
     * Functions for creating temporary per-request derived data *
     *************************************************************/

    fn parse_context(&self) -> RevsetParseContext {
        parse_context(&self.settings, &self.workspace, &self.aliases_map)
    }

    fn resolver(&self) -> DefaultSymbolResolver {
        let commit_id_resolver: revset::PrefixResolver<CommitId> =
            Box::new(|repo, prefix| self.prefix_context.resolve_commit_prefix(repo, prefix));
        let change_id_resolver: revset::PrefixResolver<Vec<CommitId>> =
            Box::new(|repo, prefix| self.prefix_context.resolve_change_prefix(repo, prefix));
        DefaultSymbolResolver::new(self.operation.repo.as_ref())
            .with_commit_id_resolver(commit_id_resolver)
            .with_change_id_resolver(change_id_resolver)
    }

    /************************************
     * IPC-message formatting functions *
     ************************************/

    pub fn format_config(&self) -> messages::RepoConfig {
        let default_query = self.settings.default_revset();
        let latest_query = self
            .session
            .latest_query
            .as_ref()
            .unwrap_or_else(|| &default_query)
            .clone();

        messages::RepoConfig::Workspace {
            absolute_path: self.workspace.workspace_root().into(),
            status: self.format_status(),
            default_query,
            latest_query,
        }
    }

    pub fn format_status(&self) -> messages::RepoStatus {
        messages::RepoStatus {
            operation_description: self
                .operation
                .repo
                .operation()
                .store_operation()
                .metadata
                .description
                .clone(),
            working_copy: self.format_commit_id(&self.operation.wc_id),
        }
    }

    pub fn format_commit_id(&self, id: &CommitId) -> messages::RevId {
        let mut hex = id.hex();
        let prefix_len = self
            .prefix_context
            .shortest_commit_prefix_len(self.operation.repo.as_ref(), id);
        let rest = hex.split_off(prefix_len);
        messages::RevId { prefix: hex, rest }
    }

    fn format_change_id(&self, id: &ChangeId) -> messages::RevId {
        let mut hex = to_reverse_hex(&id.hex()).expect("format change id as reverse hex");
        let prefix_len = self
            .prefix_context
            .shortest_change_prefix_len(self.operation.repo.as_ref(), id);
        let rest = hex.split_off(prefix_len);
        messages::RevId { prefix: hex, rest }
    }

    pub fn format_header(&self, commit: &Commit) -> Result<messages::RevHeader> {
        let index = self.operation.branches_index();
        let branches = index.get(commit.id()).iter().cloned().collect();

        Ok(messages::RevHeader {
            change_id: self.format_change_id(commit.change_id()),
            commit_id: self.format_commit_id(commit.id()),
            description: commit.description().into(),
            has_conflict: commit.has_conflict()?,
            is_working_copy: *commit.id() == self.operation.wc_id,
            branches,
        })
    }
}

impl SessionOperation {
    pub fn branches_index(&self) -> &Rc<RefNamesIndex> {
        self.branches_index
            .get_or_init(|| Rc::new(self.build_branches_index()))
    }

    fn build_branches_index(&self) -> RefNamesIndex {
        let mut index = RefNamesIndex::default();
        for (branch_name, branch_target) in self.repo.view().branches() {
            let local_target = branch_target.local_target;
            let remote_refs = branch_target.remote_refs;
            if local_target.is_present() {
                let ref_name = messages::RefName {
                    name: branch_name.to_owned(),
                    remote: None,
                    has_conflict: local_target.has_conflict(),
                    is_synced: remote_refs.iter().all(|&(_, remote_ref)| {
                        !remote_ref.is_tracking() || remote_ref.target == *local_target
                    }),
                };
                index.insert(local_target.added_ids(), ref_name);
            }
            for &(remote_name, remote_ref) in &remote_refs {
                let ref_name = messages::RefName {
                    name: branch_name.to_owned(),
                    remote: Some(remote_name.to_owned()),
                    has_conflict: remote_ref.target.has_conflict(),
                    is_synced: remote_ref.is_tracking() && remote_ref.target == *local_target,
                };
                index.insert(remote_ref.target.added_ids(), ref_name);
            }
        }
        index
    }
}

fn find_workspace_dir(cwd: &Path) -> &Path {
    cwd.ancestors()
        .find(|path| path.join(".jj").is_dir())
        .unwrap_or(cwd)
}

fn load_revset_aliases(layered_configs: &LayeredConfigs) -> Result<RevsetAliasesMap> {
    const TABLE_KEY: &str = "revset-aliases";
    let mut aliases_map = RevsetAliasesMap::new();
    // Load from all config layers in order. 'f(x)' in default layer should be
    // overridden by 'f(a)' in user.
    for (_, config) in layered_configs.sources() {
        let table = if let Some(table) = config.get_table(TABLE_KEY).optional()? {
            table
        } else {
            continue;
        };
        for (decl, value) in table.into_iter().sorted_by(|a, b| a.0.cmp(&b.0)) {
            value
                .into_string()
                .map_err(|e| anyhow!(e))
                .and_then(|v| aliases_map.insert(&decl, v).map_err(|e| anyhow!(e)))?;
        }
    }
    Ok(aliases_map)
}

fn parse_context<'a>(
    settings: &UserSettings,
    workspace: &'a Workspace,
    aliases_map: &'a RevsetAliasesMap,
) -> RevsetParseContext<'a> {
    let workspace_context = RevsetWorkspaceContext {
        cwd: workspace.workspace_root(),
        workspace_id: workspace.workspace_id(),
        workspace_root: workspace.workspace_root(),
    };
    RevsetParseContext {
        aliases_map: &aliases_map,
        user_email: settings.user_email(),
        workspace: Some(workspace_context),
    }
}

fn parse_revset(
    parse_context: &RevsetParseContext,
    revision: &str,
) -> Result<Rc<RevsetExpression>> {
    let expression = revset::parse(revision, parse_context).context("parse revset")?;
    let expression = revset::optimize(expression);
    Ok(expression)
}

/*************************/
/* from commit_templater */
/*************************/

#[derive(Default)]
pub struct RefNamesIndex {
    index: HashMap<CommitId, Vec<messages::RefName>>,
}

impl RefNamesIndex {
    fn insert<'a>(&mut self, ids: impl IntoIterator<Item = &'a CommitId>, name: messages::RefName) {
        for id in ids {
            let ref_names = self.index.entry(id.clone()).or_default();
            ref_names.push(name.clone());
        }
    }

    fn get(&self, id: &CommitId) -> &[messages::RefName] {
        if let Some(names) = self.index.get(id) {
            names
        } else {
            &[]
        }
    }
}
