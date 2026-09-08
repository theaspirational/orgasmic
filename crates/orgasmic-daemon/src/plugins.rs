//! Cached P2 registry snapshots. Only reconciliation reads manifests; node
//! requests borrow a project snapshot and use the existing writer and hooks.
use crate::node_types::NodeTypeRegistry;
use anyhow::{Context, Result};
use orgasmic_core::plugin::{
    read_json, write_json, PluginActivation, PluginActivations, PluginManifest,
};
use orgasmic_core::{Home, NodeTypeDescriptor};
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

#[derive(Clone)]
pub struct ProjectPlugins {
    pub registry: Arc<NodeTypeRegistry>,
    pub active: BTreeMap<String, PluginManifest>,
    pub owners: BTreeMap<String, NodeTypeDescriptor>,
    pub errors: BTreeMap<String, String>,
}

impl ProjectPlugins {
    pub fn owner(&self, collection: &str) -> Option<&str> {
        self.owners
            .iter()
            .find(|(_, d)| d.collection == collection)
            .map(|(id, _)| id.as_str())
    }
    pub fn descriptor(&self, collection: &str) -> Option<&NodeTypeDescriptor> {
        if self
            .owner(collection)
            .is_some_and(|id| !self.active.contains_key(id))
        {
            return None;
        }
        self.registry.descriptor(collection)
    }
    pub fn check_write(&self, collection: &str, source: Option<&str>) -> Result<()> {
        if let Some(id) = self.owner(collection) {
            let manifest = self.active.get(id).with_context(|| {
                format!("plugin {id} is unavailable or disabled; node is read-only")
            })?;
            if let Some(source) = source {
                let header = source
                    .lines()
                    .take_while(|line| !line.starts_with("* "))
                    .find_map(|line| line.strip_prefix("#+plugin: "));
                let valid =
                    header
                        .and_then(|s| s.split_once(" schema="))
                        .is_some_and(|(owner, schema)| {
                            owner == id
                                && schema
                                    .parse::<u32>()
                                    .ok()
                                    .is_some_and(|v| manifest.schema_accepts.contains(&v))
                        });
                anyhow::ensure!(
                    valid,
                    "plugin {id} schema mismatch; node is read-only until migrated"
                );
            }
        }
        Ok(())
    }
    pub fn header(&self, collection: &str) -> Option<String> {
        let id = self.owner(collection)?;
        let manifest = self.active.get(id)?;
        Some(format!("#+plugin: {id} schema={}\n", manifest.schema))
    }
}

#[derive(Clone, Serialize)]
pub struct PluginStatus {
    pub id: String,
    pub manifest: Option<PluginManifest>,
    pub enabled: bool,
    pub error: Option<String>,
}

type FileSignature = Vec<(PathBuf, Option<(std::time::SystemTime, u64)>)>;

struct Cached {
    signature: FileSignature,
    base: NodeTypeRegistry,
    installed: BTreeMap<String, PluginManifest>,
    errors: BTreeMap<String, String>,
    owners: BTreeMap<String, NodeTypeDescriptor>,
    projects: BTreeMap<PathBuf, Arc<ProjectPlugins>>,
    leases: BTreeMap<String, RunLease>,
}

struct RunLease {
    token_hash: Vec<u8>,
    root: PathBuf,
    project: String,
    manifest: PluginManifest,
    caller: crate::authz::Identity,
    expires: std::time::Instant,
}

pub struct PluginRegistry {
    home: Home,
    data: Mutex<Cached>,
    /// Reconciliation/disable waits for in-flight generic writes to finish.
    pub operations: Arc<tokio::sync::RwLock<()>>,
}

impl PluginRegistry {
    pub fn new(home: &Home) -> Result<Arc<Self>> {
        let registry = Arc::new(Self {
            home: home.clone(),
            data: Mutex::new(Cached {
                signature: Vec::new(),
                base: crate::node_types::load(home)?,
                installed: BTreeMap::new(),
                errors: BTreeMap::new(),
                owners: read_json(&home.user().join("plugin-ownership.json"))?,
                projects: BTreeMap::new(),
                leases: BTreeMap::new(),
            }),
            operations: Arc::new(tokio::sync::RwLock::new(())),
        });
        registry.reconcile(true)?;
        Ok(registry)
    }

    pub fn start(self: &Arc<Self>, events: crate::events::EventBus) {
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                let Some(registry) = weak.upgrade() else {
                    break;
                };
                match registry.reconcile_if_changed().await {
                    Ok(true) => events.publish(
                        crate::events::Topic::Board,
                        crate::events::EventPayload::BoardRefreshed,
                    ),
                    Ok(false) => {}
                    Err(error) => tracing::warn!(%error, "plugin reconciliation failed"),
                }
            }
        });
    }

    async fn reconcile_if_changed(&self) -> Result<bool> {
        {
            let data = self.data.lock().unwrap();
            if self.signature(&data)? == data.signature {
                return Ok(false);
            }
        }
        let _guard = self.operations.write().await;
        // Recheck after waiting: another reconciliation may have won the race.
        self.reconcile(false)
    }

    fn signature(&self, data: &Cached) -> Result<FileSignature> {
        let mut paths = vec![self.home.user().join("plugin-ownership.json")];
        for root in [
            self.home.user().join("schema/node-types"),
            self.home.source().join("shipped/schema/node-types"),
            self.home.user().join("plugins"),
        ] {
            paths.push(root.clone());
            if let Ok(entries) = std::fs::read_dir(&root) {
                for entry in entries {
                    let entry = entry?;
                    let path = if root.ends_with("plugins") {
                        entry.path().join("plugin.org")
                    } else {
                        entry.path()
                    };
                    if path.extension().is_some_and(|v| v == "org") {
                        paths.push(path);
                    }
                }
            }
        }
        paths.extend(
            data.projects
                .keys()
                .map(|root| root.join(".orgasmic/plugins.json")),
        );
        paths.sort();
        Ok(paths
            .into_iter()
            .map(|path| {
                let stamp = path
                    .metadata()
                    .ok()
                    .and_then(|m| m.modified().ok().map(|time| (time, m.len())));
                (path, stamp)
            })
            .collect())
    }

    pub fn reconcile(&self, force: bool) -> Result<bool> {
        let mut data = self.data.lock().unwrap();
        let signature = self.signature(&data)?;
        if !force && signature == data.signature {
            return Ok(false);
        }
        // Reject an invalid legacy descriptor update without discarding the
        // last good base registry or breaking unrelated plugin collections.
        let mut errors = BTreeMap::new();
        match crate::node_types::load(&self.home).and_then(|base| {
            let mut combined = base.clone();
            for descriptor in data.owners.values() {
                combined.register(descriptor.clone(), false)?;
            }
            Ok(base)
        }) {
            Ok(base) => data.base = base,
            Err(error) => {
                errors.insert("legacy-descriptors".into(), error.to_string());
            }
        }
        let mut installed = BTreeMap::new();
        if let Ok(entries) = std::fs::read_dir(self.home.user().join("plugins")) {
            for entry in entries {
                let entry = entry?;
                let id = entry.file_name().to_string_lossy().to_string();
                if id.starts_with('.') {
                    continue;
                }
                let result = (|| {
                    orgasmic_core::plugin::validate_id(&id)?;
                    let manifest = PluginManifest::read_dir(&entry.path())?;
                    anyhow::ensure!(manifest.id == id, "directory name must match plugin id");
                    anyhow::ensure!(
                        data.base.descriptor(&id).is_none(),
                        "plugin id is reserved by core"
                    );
                    Ok(manifest)
                })();
                match result {
                    Ok(manifest) => {
                        installed.insert(id, manifest);
                    }
                    Err(error) => {
                        errors.insert(id, format!("{error:#}"));
                    }
                }
            }
        }
        data.installed = installed;
        data.errors = errors;
        data.owners = read_json(&self.home.user().join("plugin-ownership.json"))?;
        let roots: Vec<_> = data.projects.keys().cloned().collect();
        for root in roots {
            let snapshot = Self::build(&data, &root)?;
            data.projects.insert(root, Arc::new(snapshot));
        }
        let projects = data.projects.clone();
        data.leases.retain(|_, lease| {
            lease.expires > std::time::Instant::now()
                && projects
                    .get(&lease.root)
                    .and_then(|p| p.active.get(&lease.manifest.id))
                    == Some(&lease.manifest)
        });
        data.signature = signature;
        Ok(true)
    }

    fn build(data: &Cached, root: &Path) -> Result<ProjectPlugins> {
        let mut registry = data.base.clone();
        let mut errors = data.errors.clone();
        let activations: PluginActivations = match read_json(&root.join(".orgasmic/plugins.json")) {
            Ok(activations) => activations,
            Err(error) => {
                errors.insert("activation".into(), error.to_string());
                BTreeMap::new()
            }
        };
        let mut active = BTreeMap::new();
        for (id, descriptor) in &data.owners {
            registry.register(descriptor.clone(), false)?;
            if let (Some(manifest), Some(activation)) =
                (data.installed.get(id), activations.get(id))
            {
                if activation.enabled {
                    if manifest.node_type.as_ref().is_none_or(|d| {
                        d.collection != descriptor.collection || d.id_prefix != descriptor.id_prefix
                    }) {
                        errors.insert(
                            id.clone(),
                            "collection and prefix ownership cannot change".into(),
                        );
                    } else if !manifest
                        .capabilities
                        .is_subset(&activation.approved_capabilities)
                    {
                        errors.insert(
                            id.clone(),
                            "capabilities grew; enable again to approve them".into(),
                        );
                    } else {
                        if let Some(descriptor) = &manifest.node_type {
                            registry.register(descriptor.clone(), true)?;
                        }
                        active.insert(id.clone(), manifest.clone());
                    }
                }
            }
        }
        for (id, manifest) in &data.installed {
            if manifest.node_type.is_none()
                && !data.owners.contains_key(id)
                && activations.get(id).is_some_and(|a| {
                    a.enabled && manifest.capabilities.is_subset(&a.approved_capabilities)
                })
            {
                active.insert(id.clone(), manifest.clone());
            }
        }
        Ok(ProjectPlugins {
            registry: Arc::new(registry),
            active,
            owners: data.owners.clone(),
            errors,
        })
    }

    pub fn project(&self, root: &Path) -> Result<Arc<ProjectPlugins>> {
        let mut data = self.data.lock().unwrap();
        if let Some(snapshot) = data.projects.get(root) {
            return Ok(snapshot.clone());
        }
        let snapshot = Arc::new(Self::build(&data, root)?);
        data.projects.insert(root.to_path_buf(), snapshot.clone());
        Ok(snapshot)
    }

    pub fn list(&self, root: &Path) -> Result<Vec<PluginStatus>> {
        let snapshot = self.project(root)?;
        let data = self.data.lock().unwrap();
        let ids: std::collections::BTreeSet<_> = data
            .installed
            .keys()
            .chain(data.owners.keys())
            .chain(snapshot.errors.keys())
            .cloned()
            .collect();
        Ok(ids
            .into_iter()
            .map(|id| PluginStatus {
                manifest: data.installed.get(&id).cloned(),
                enabled: snapshot.active.contains_key(&id),
                error: snapshot.errors.get(&id).cloned(),
                id,
            })
            .collect())
    }

    pub fn issue(
        &self,
        root: &Path,
        project: &str,
        id: &str,
        command: &str,
        caller: &crate::authz::Identity,
    ) -> Result<(String, String)> {
        use crate::authz::{require, Action, Identity};
        anyhow::ensure!(
            !matches!(caller, Identity::Plugin { .. }),
            "plugins cannot mint credentials"
        );
        let snapshot = self.project(root)?;
        let manifest = snapshot
            .active
            .get(id)
            .context("plugin is disabled or unavailable")?;
        for capability in &manifest.capabilities {
            require(
                caller,
                Some(project),
                if capability == "nodes.write" {
                    Action::NodesWrite
                } else {
                    Action::GraphRead
                },
            )?;
        }
        manifest.command_path(&self.home.user().join("plugins").join(id), command)?;
        let mut data = self.data.lock().unwrap();
        let now = std::time::Instant::now();
        data.leases.retain(|_, lease| lease.expires > now);
        anyhow::ensure!(data.leases.len() < 1024, "too many active plugin commands");
        let lease = uuid::Uuid::new_v4().to_string();
        let token = format!("plugin-{}", uuid::Uuid::new_v4());
        data.leases.insert(
            lease.clone(),
            RunLease {
                token_hash: token_hash(&token),
                root: root.into(),
                project: project.into(),
                manifest: manifest.clone(),
                caller: caller.clone(),
                expires: now + std::time::Duration::from_secs(24 * 60 * 60),
            },
        );
        Ok((lease, token))
    }

    pub fn revoke(&self, lease: &str, caller: &crate::authz::Identity) -> Result<()> {
        let mut data = self.data.lock().unwrap();
        if let Some(run) = data.leases.get(lease) {
            anyhow::ensure!(
                matches!(caller, crate::authz::Identity::Admin)
                    || caller.member_name() == run.caller.member_name(),
                "credential belongs to another caller"
            );
        }
        data.leases.remove(lease);
        Ok(())
    }

    pub fn remove(&self, id: &str, roots: &[PathBuf]) -> Result<PathBuf> {
        orgasmic_core::plugin::validate_id(id)?;
        let path = self.home.user().join("plugins").join(id);
        anyhow::ensure!(
            path.is_dir() && !path.symlink_metadata()?.file_type().is_symlink(),
            "plugin is not an installed directory"
        );
        // Disable in every registered ledger before removing code. On any
        // error the remaining installation is disabled, never silently live.
        for root in roots {
            let path = root.join(".orgasmic/plugins.json");
            let mut activations: PluginActivations = read_json(&path)?;
            if let Some(activation) = activations.get_mut(id) {
                activation.enabled = false;
                write_json(&path, &activations)?;
            }
        }
        let removed = self.home.user().join("plugins-removed");
        std::fs::create_dir_all(&removed)?;
        let backup = removed.join(format!("{id}-{}", uuid::Uuid::new_v4()));
        std::fs::rename(path, &backup)?;
        self.reconcile(true)?;
        Ok(backup)
    }

    pub fn identity(&self, token: &str) -> Option<crate::authz::Identity> {
        use crate::authz::Identity;
        use subtle::ConstantTimeEq;
        let hash = token_hash(token);
        let data = self.data.lock().unwrap();
        let lease = data
            .leases
            .values()
            .find(|lease| bool::from(lease.token_hash.ct_eq(&hash)))?;
        if lease.expires <= std::time::Instant::now() {
            return None;
        }
        let snapshot = data.projects.get(&lease.root)?;
        let manifest = snapshot.active.get(&lease.manifest.id)?;
        if manifest != &lease.manifest {
            return None;
        }
        let caller = match &lease.caller {
            Identity::Member { name, .. } => {
                let entry = orgasmic_core::find_member_by_name(&self.home, name).ok()??;
                Identity::Member {
                    name: entry.name,
                    grants: entry.grants,
                }
            }
            Identity::Admin => Identity::Admin,
            Identity::Plugin { .. } => return None,
        };
        Some(Identity::Plugin {
            id: manifest.id.clone(),
            project: lease.project.clone(),
            capabilities: manifest.capabilities.clone(),
            caller: Box::new(caller),
        })
    }

    /// Called under the operation write guard by the admin-only API.
    pub fn activate(
        &self,
        root: &Path,
        id: &str,
        enabled: bool,
        approved: &std::collections::BTreeSet<String>,
    ) -> Result<()> {
        orgasmic_core::plugin::validate_id(id)?;
        self.reconcile(true)?;
        {
            let mut data = self.data.lock().unwrap();
            let mut activations: PluginActivations =
                read_json(&root.join(".orgasmic/plugins.json"))?;
            if enabled {
                let manifest = data
                    .installed
                    .get(id)
                    .context("plugin is not installed or invalid")?
                    .clone();
                anyhow::ensure!(
                    manifest.capabilities == *approved,
                    "approve the current manifest capabilities before enabling"
                );
                if let Some(descriptor) = &manifest.node_type {
                    if let Some(previous) = data.owners.get(id) {
                        anyhow::ensure!(
                            previous.collection == descriptor.collection
                                && previous.id_prefix == descriptor.id_prefix,
                            "plugin ownership cannot change"
                        );
                    }
                    let mut registry = data.base.clone();
                    for (owner, descriptor) in &data.owners {
                        if owner != id {
                            registry.register(descriptor.clone(), false)?;
                        }
                    }
                    registry.register(descriptor.clone(), false)?;
                    data.owners.insert(id.into(), descriptor.clone());
                    write_json(
                        &self.home.user().join("plugin-ownership.json"),
                        &data.owners,
                    )?;
                }
                activations.insert(
                    id.into(),
                    PluginActivation {
                        enabled,
                        approved_capabilities: approved.clone(),
                    },
                );
            } else {
                activations.entry(id.into()).or_default().enabled = false;
            }
            write_json(&root.join(".orgasmic/plugins.json"), &activations)?;
            data.projects.entry(root.to_path_buf()).or_insert_with(|| {
                Arc::new(ProjectPlugins {
                    registry: Arc::new(NodeTypeRegistry::default()),
                    active: BTreeMap::new(),
                    owners: BTreeMap::new(),
                    errors: BTreeMap::new(),
                })
            });
        }
        self.reconcile(true).map(|_| ())
    }
}

fn token_hash(token: &str) -> Vec<u8> {
    use sha2::{Digest, Sha256};
    Sha256::digest(token.as_bytes()).to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn unchanged_reconciliation_does_not_wait_for_node_writes() {
        let temp = tempfile::tempdir().unwrap();
        let home = Home::at(temp.path().join("home"));
        home.ensure().unwrap();
        let registry = PluginRegistry::new(&home).unwrap();
        let _node_write = registry.operations.read().await;
        assert!(!tokio::time::timeout(
            std::time::Duration::from_secs(1),
            registry.reconcile_if_changed(),
        )
        .await
        .expect("unchanged reconciliation must not queue an operations writer")
        .unwrap());
        assert!(registry.operations.try_read().is_ok());
    }

    #[test]
    fn snapshots_are_cached_until_reconciliation_and_legacy_types_reload() {
        let temp = tempfile::tempdir().unwrap();
        let home = Home::at(temp.path().join("home"));
        home.ensure().unwrap();
        let root = temp.path().join("project");
        let registry = PluginRegistry::new(&home).unwrap();
        let before = registry.project(&root).unwrap();
        let folder = home.user().join("schema/node-types");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("meetings.org"), "* Node type\n:PROPERTIES:\n:COLLECTION: meetings\n:ID_PREFIX: MEET-\n:LABEL: Meeting\n:LABEL_PLURAL: Meetings\n:END:\n").unwrap();
        // Request path returns the same Arc even when files changed.
        assert!(Arc::ptr_eq(&before, &registry.project(&root).unwrap()));
        assert!(registry.reconcile(false).unwrap());
        let after = registry.project(&root).unwrap();
        assert!(after.registry.descriptor("meetings").is_some());
        assert!(before.registry.descriptor("meetings").is_none());
        assert!(!registry.reconcile(false).unwrap());
        assert!(Arc::ptr_eq(&after, &registry.project(&root).unwrap()));
    }
}
