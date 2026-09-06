//! TASK-ZZ2S4: offline, resumable identity repair. Historical event bytes never change.
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, ensure, Context, Result};
use orgasmic_core::id::{is_valid_task_path_id, mint_node_id, NodeIdClass};
use orgasmic_core::{projects, read_claims, Home, OrgFile, ProjectFile, TaskHeading};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Serialize, Deserialize)]
struct PlannedFile {
    source: String,
    target: String,
    sha256: String,
    mode: String,
    oid: String,
    after: Option<String>,
    after_oid: String,
}

#[derive(Serialize, Deserialize)]
struct Plan {
    version: u32,
    root: PathBuf,
    project: String,
    checkpoint: String,
    mapping: BTreeMap<String, String>,
    files: Vec<PlannedFile>,
    record_path: String,
    record: String,
    record_oid: String,
}

pub(crate) fn run(home: &Home, apply: bool) -> Result<()> {
    let root = crate::manager::find_project_root()?;
    run_at(home, &root, apply)
}

fn run_at(home: &Home, root: &Path, apply: bool) -> Result<()> {
    safe_absolute(&home.root)?;
    safe_absolute(root)?;
    let root = root.canonicalize()?;
    // Do not contact/autostart the daemon. The same lifetime lock used by the
    // daemon closes the check/stop race; only the operator may stop its service.
    let _lock = if apply {
        let path = home.root.join("daemon.lock");
        if present(&path)? {
            safe_absolute(&path)?;
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        fs2::FileExt::try_lock_exclusive(&file)
            .context("daemon is running or its lock is busy; stop the daemon before --apply")?;
        Some(file)
    } else {
        None
    };
    let marker = home.task_id_repair_plan();
    let plan = if present(&marker)? {
        safe_absolute(&marker)?;
        let plan: Plan = serde_json::from_slice(&fs::read(&marker)?)?;
        ensure!(
            plan.version == 1 && plan.root == root,
            "maintenance plan belongs to another ledger/version"
        );
        verify_registration(home, &root, &plan.project)?;
        validate_plan(&plan)?;
        verify_state(&plan, false)?;
        plan
    } else {
        let plan = plan(home, &root)?;
        if plan.mapping.is_empty() {
            println!("No malformed task IDs; nothing to repair.");
            return Ok(());
        }
        plan
    };
    println!(
        "{} task-ID repair at {}",
        if apply { "APPLY" } else { "DRY RUN" },
        root.display()
    );
    for (old, new) in &plan.mapping {
        println!("  {old} -> {new}");
    }
    println!(
        "  checkpoint {}\n  record {}",
        plan.checkpoint, plan.record_path
    );
    if !apply {
        return Ok(());
    }
    if !present(&marker)? {
        // Install the complete plan and fsync its directory BEFORE moving a
        // source. The daemon checks the marker both before and under its lock.
        atomic_write(&marker, &serde_json::to_vec_pretty(&plan)?, None)?;
    }
    apply_plan(&plan, None)?;
    fs::remove_file(&marker)?;
    sync_dir(&home.root)?;
    println!(
        "Repair committed locally: {}",
        git(&root, &["rev-parse", "HEAD"])?.trim()
    );
    Ok(())
}

fn verify_registration(home: &Home, root: &Path, project: &str) -> Result<()> {
    let entries = projects::read_board(home)?;
    ensure!(
        entries
            .iter()
            .any(|e| e.id == project && e.path.canonicalize().ok().as_deref() == Some(root)),
        "repair requires the registered ledger for project {project}"
    );
    ensure!(
        home.project_ledger(project).canonicalize()? == root,
        "repair requires the actual home ledger"
    );
    Ok(())
}

fn git(root: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .arg("--no-optional-locks")
        .args(args)
        .current_dir(root)
        .output()?;
    ensure!(
        out.status.success(),
        "git {}: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(String::from_utf8(out.stdout)?)
}

fn blob_oid(root: &Path, bytes: &[u8]) -> Result<String> {
    let mut child = Command::new("git")
        .args(["hash-object", "--stdin"])
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    child.stdin.take().context("git stdin")?.write_all(bytes)?;
    let out = child.wait_with_output()?;
    ensure!(out.status.success(), "git hash-object failed");
    Ok(String::from_utf8(out.stdout)?.trim().into())
}

fn present(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn safe_relative(path: &str) -> Result<()> {
    ensure!(
        !path.is_empty()
            && Path::new(path)
                .components()
                .all(|c| matches!(c, Component::Normal(_))),
        "unsafe repair path {path:?}"
    );
    Ok(())
}

fn safe_absolute(path: &Path) -> Result<()> {
    ensure!(path.is_absolute(), "repair requires absolute paths");
    let mut part = PathBuf::new();
    for component in path.components() {
        ensure!(
            !matches!(component, Component::ParentDir),
            "unsafe path {}",
            path.display()
        );
        part.push(component);
        let metadata = fs::symlink_metadata(&part)?;
        ensure!(
            !metadata.file_type().is_symlink(),
            "symlink refused: {}",
            part.display()
        );
        ensure!(
            metadata.is_file() || metadata.is_dir(),
            "special file refused: {}",
            part.display()
        );
    }
    Ok(())
}

fn no_git_operation(root: &Path) -> Result<()> {
    for name in [
        "MERGE_HEAD",
        "REBASE_HEAD",
        "CHERRY_PICK_HEAD",
        "REVERT_HEAD",
        "rebase-merge",
        "rebase-apply",
        "sequencer",
        "index.lock",
    ] {
        let value = git(root, &["rev-parse", "--git-path", name])?;
        ensure!(
            !root.join(value.trim()).try_exists()?,
            "git operation in progress: {name}"
        );
    }
    ensure!(
        git(root, &["rev-parse", "--show-toplevel"])?.trim()
            == root.to_str().context("non-UTF8 ledger root")?,
        "ledger is not the git root"
    );
    Ok(())
}

fn index(root: &Path) -> Result<BTreeMap<String, (String, String)>> {
    let source = git(root, &["ls-files", "--stage", "-z"])?;
    let mut result = BTreeMap::new();
    for line in source.split('\0').filter(|s| !s.is_empty()) {
        let (meta, path) = line.split_once('\t').context("invalid git index row")?;
        safe_relative(path)?;
        let fields = meta.split_whitespace().collect::<Vec<_>>();
        ensure!(
            fields.len() == 3 && fields[2] == "0" && matches!(fields[0], "100644" | "100755"),
            "unmerged or non-regular index entry: {path}"
        );
        ensure!(
            result
                .insert(path.into(), (fields[0].into(), fields[1].into()))
                .is_none(),
            "duplicate index path"
        );
    }
    Ok(result)
}

fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn current_file(path: &str) -> bool {
    let p = Path::new(path);
    [
        ".orgasmic/tasks",
        ".orgasmic/decisions",
        ".orgasmic/glossary",
    ]
    .iter()
    .any(|collection| {
        p.starts_with(collection)
            && (p.file_name().is_some_and(|n| n == "node.org")
                || (p.parent() == Some(Path::new(collection))
                    && p.extension().is_some_and(|x| x == "org")))
    })
}

// Exact local IDs only: no descendant/prefix matches and no foreign-project refs.
fn replace_refs(source: &str, mapping: &BTreeMap<String, String>, project: &str) -> String {
    fn token(c: char) -> bool {
        c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.')
    }
    let mut out = source.to_string();
    for (old, new) in mapping {
        let mut next = String::new();
        let mut end = 0;
        for (start, _) in out.match_indices(old) {
            let stop = start + old.len();
            let suffix = &out[stop..];
            let continuation = suffix.chars().next().is_some_and(|c| token(c) && c != '.')
                || (suffix.starts_with('.') && suffix[1..].chars().next().is_some_and(token));
            if out[..start].chars().next_back().is_some_and(token) || continuation {
                continue;
            }
            if out[..start].ends_with(':') {
                let prefix = out[..start - 1].rsplit(|c| !token(c)).next().unwrap_or("");
                if !prefix.is_empty() && prefix != project {
                    continue;
                }
            }
            next.push_str(&out[end..start]);
            next.push_str(new);
            end = stop;
        }
        next.push_str(&out[end..]);
        out = next;
    }
    out
}

fn plan(home: &Home, root: &Path) -> Result<Plan> {
    no_git_operation(root)?;
    ensure!(
        git(root, &["status", "--porcelain=v1", "--untracked-files=all"])?.is_empty(),
        "refusing dirty ledger"
    );
    let org = OrgFile::parse(
        fs::read_to_string(root.join(".orgasmic/project.org"))?,
        "project.org",
    )?;
    let project = ProjectFile::from_org(&org, "project.org")?.id.to_owned();
    verify_registration(home, root, &project)?;
    let entries = index(root)?;
    let mut mapping = BTreeMap::new();
    let mut ids = BTreeSet::new();
    let mut parents = Vec::new();
    for path in entries.keys() {
        safe_absolute(&root.join(path))?;
        if !path.starts_with(".orgasmic/tasks/TASK-") || !path.ends_with("/node.org") {
            continue;
        }
        let dir = Path::new(path).parent().context("task directory")?;
        ensure!(
            dir.parent() == Some(Path::new(".orgasmic/tasks")),
            "unexpected nested task node {path}"
        );
        let id = dir
            .file_name()
            .context("task id")?
            .to_str()
            .context("non UTF8 task id")?;
        let file = OrgFile::parse(fs::read_to_string(root.join(path))?, path)?;
        let [heading] = file.headings.as_slice() else {
            bail!("expected one task heading: {path}");
        };
        ensure!(
            heading.property("ID") == Some(id),
            "task directory/identity mismatch: {path}"
        );
        ids.insert(id.to_owned());
        if let Some(parent) = heading.property("PARENT") {
            parents.push(parent.to_owned());
        }
        if is_valid_task_path_id(id) {
            continue;
        }
        ensure!(
            id.starts_with("TASK-")
                && id.len() == 10
                && id[5..]
                    .bytes()
                    .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit()),
            "unsupported malformed task identity {id}"
        );
        ensure!(
            matches!(heading.todo.as_deref(), Some("DONE" | "CANCELLED")),
            "malformed task is not terminal: {id}"
        );
        ensure!(
            heading.title.split_whitespace().next() == Some(id),
            "malformed task title identity mismatch: {id}"
        );
        let journal = format!("{}/journal.org", dir.display());
        ensure!(
            entries.contains_key(&journal),
            "missing tracked journal for {id}"
        );
        mapping.insert(id.to_owned(), String::new());
    }
    let claims = read_claims(root)?;
    for old in mapping.keys() {
        ensure!(
            !ids.iter().any(|id| id.starts_with(&format!("{old}.")))
                && !parents.iter().any(|id| id == old),
            "malformed task has descendants: {old}"
        );
        ensure!(
            !claims
                .keys()
                .any(|id| id == old || id.starts_with(&format!("{old}."))),
            "active claim for malformed task {old}"
        );
    }
    for new in mapping.values_mut() {
        loop {
            let candidate = mint_node_id(NodeIdClass::Task);
            if ids.insert(candidate.clone())
                && !root.join(".orgasmic/tasks").join(&candidate).exists()
            {
                *new = candidate;
                break;
            }
        }
    }
    let checkpoint = git(root, &["rev-parse", "HEAD"])?.trim().to_string();
    let mut files = Vec::new();
    let mut journals = BTreeMap::new();
    for (source, (mode, oid)) in entries {
        let bytes = fs::read(root.join(&source))?;
        let sha256 = hash(&bytes);
        let mut target = source.clone();
        for (old, new) in &mapping {
            let prefix = format!(".orgasmic/tasks/{old}/");
            if let Some(tail) = source.strip_prefix(&prefix) {
                target = format!(".orgasmic/tasks/{new}/{tail}");
            }
        }
        let after = if current_file(&source) {
            let before = String::from_utf8(bytes.clone())?;
            let changed = replace_refs(&before, &mapping, &project);
            (changed != before).then_some(changed)
        } else {
            None
        };
        if source.ends_with("/journal.org") {
            journals.insert(source.clone(), sha256.clone());
        }
        let after_oid = match &after {
            Some(text) => blob_oid(root, text.as_bytes())?,
            None => oid.clone(),
        };
        files.push(PlannedFile {
            source,
            target,
            sha256,
            mode,
            oid,
            after,
            after_oid,
        });
    }
    let record_path = format!(".orgasmic/repairs/task-ids-{}.json", uuid::Uuid::new_v4());
    let record = serde_json::to_string_pretty(&serde_json::json!({
        "version": 1, "project": project, "checkpoint": checkpoint, "mapping": mapping,
        "journal_sha256": journals, "history_policy": "Original journals, claims and transaction events retain their original IDs and bytes; mapping records lineage."
    }))? + "\n";
    let record_oid = blob_oid(root, record.as_bytes())?;
    let plan = Plan {
        version: 1,
        root: root.to_owned(),
        project,
        checkpoint,
        mapping,
        files,
        record_path,
        record,
        record_oid,
    };
    validate_plan(&plan)?;
    ensure!(
        git(root, &["status", "--porcelain=v1", "--untracked-files=all"])?.is_empty(),
        "ledger changed during planning"
    );
    verify_state(&plan, false)?;
    Ok(plan)
}

fn validate_plan(plan: &Plan) -> Result<()> {
    safe_relative(&plan.record_path)?;
    ensure!(
        plan.record_path.starts_with(".orgasmic/repairs/task-ids-")
            && plan.record_path.ends_with(".json"),
        "unsafe repair record path"
    );
    ensure!(
        matches!(plan.checkpoint.len(), 40 | 64)
            && plan.checkpoint.bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid repair checkpoint"
    );
    let mut original = BTreeMap::new();
    for entry in git(&plan.root, &["ls-tree", "-r", "-z", &plan.checkpoint])?
        .split('\0')
        .filter(|s| !s.is_empty())
    {
        let (meta, path) = entry.split_once('\t').context("invalid checkpoint tree")?;
        let fields = meta.split_whitespace().collect::<Vec<_>>();
        ensure!(
            fields.len() == 3 && fields[1] == "blob",
            "unsupported checkpoint entry"
        );
        original.insert(
            path.to_owned(),
            (fields[0].to_owned(), fields[2].to_owned()),
        );
    }
    let planned = plan
        .files
        .iter()
        .map(|f| (f.source.clone(), (f.mode.clone(), f.oid.clone())))
        .collect::<BTreeMap<_, _>>();
    ensure!(
        planned.len() == plan.files.len() && planned == original,
        "plan does not match checkpoint inventory"
    );
    let mut paths = BTreeSet::new();
    for file in &plan.files {
        safe_relative(&file.source)?;
        safe_relative(&file.target)?;
        ensure!(paths.insert(file.target.clone()), "duplicate target path");
        let mut expected = file.source.clone();
        for (old, new) in &plan.mapping {
            ensure!(
                old.len() == 10
                    && old.starts_with("TASK-")
                    && old[5..].bytes().all(|b| b.is_ascii_alphanumeric())
                    && !is_valid_task_path_id(old)
                    && is_valid_task_path_id(new),
                "invalid recorded mapping"
            );
            if let Some(tail) = file.source.strip_prefix(&format!(".orgasmic/tasks/{old}/")) {
                expected = format!(".orgasmic/tasks/{new}/{tail}");
            }
        }
        if let Some(after) = &file.after {
            let before = git(
                &plan.root,
                &["show", &format!("{}:{}", plan.checkpoint, file.source)],
            )?;
            ensure!(
                hash(before.as_bytes()) == file.sha256
                    && replace_refs(&before, &plan.mapping, &plan.project) == *after
                    && blob_oid(&plan.root, after.as_bytes())? == file.after_oid,
                "recorded rewrite differs from checkpoint or identity mapping"
            );
        } else {
            ensure!(file.oid == file.after_oid, "unrecorded content change");
        }
        ensure!(
            expected == file.target && (file.after.is_none() || current_file(&file.source)),
            "unsafe recorded operation"
        );
    }
    ensure!(
        !paths.contains(&plan.record_path),
        "repair record collides with tracked file"
    );
    Ok(())
}

fn verify_state(plan: &Plan, final_state: bool) -> Result<()> {
    no_git_operation(&plan.root)?;
    let head = git(&plan.root, &["rev-parse", "HEAD"])?.trim().to_string();
    if head != plan.checkpoint {
        ensure!(
            git(&plan.root, &["rev-parse", "HEAD^"])?.trim() == plan.checkpoint,
            "HEAD moved outside recorded repair"
        );
        ensure!(
            git(&plan.root, &["show", &format!("HEAD:{}", plan.record_path)])? == plan.record,
            "HEAD is not the recorded repair commit"
        );
        ensure!(
            git(
                &plan.root,
                &["status", "--porcelain=v1", "--untracked-files=all"]
            )?
            .is_empty(),
            "committed repair tree is dirty"
        );
    }
    let mut expected_initial = BTreeMap::new();
    let mut expected_final = BTreeMap::new();
    let mut actual_paths = BTreeSet::new();
    for file in &plan.files {
        expected_initial.insert(file.source.clone(), (file.mode.clone(), file.oid.clone()));
        expected_final.insert(
            file.target.clone(),
            (file.mode.clone(), file.after_oid.clone()),
        );
        let source = plan.root.join(&file.source);
        let target = plan.root.join(&file.target);
        let path = if file.source != file.target {
            ensure!(
                present(&source)? != present(&target)?,
                "missing or duplicated repair source: {}",
                file.source
            );
            if source.exists() {
                ensure!(!final_state, "unmoved task directory");
                source
            } else {
                target
            }
        } else {
            source
        };
        safe_absolute(&path)?;
        ensure!(fs::metadata(&path)?.is_file(), "non-file repair source");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let executable = fs::metadata(&path)?.permissions().mode() & 0o111 != 0;
            ensure!(
                executable == (file.mode == "100755"),
                "unexpected file mode in {}",
                path.display()
            );
        }
        let bytes = fs::read(&path)?;
        let before = hash(&bytes) == file.sha256;
        let after = file
            .after
            .as_ref()
            .is_some_and(|text| bytes == text.as_bytes());
        ensure!(
            (before && (!final_state || file.after.is_none())) || after,
            "unexpected edit in {}",
            path.display()
        );
        actual_paths.insert(
            path.strip_prefix(&plan.root)?
                .to_str()
                .context("path UTF8")?
                .to_string(),
        );
    }
    let record = plan.root.join(&plan.record_path);
    if let Some(parent) = record.parent().filter(|p| p.exists()) {
        safe_absolute(parent)?;
    }
    if present(&record)? {
        safe_absolute(&record)?;
        ensure!(
            fs::read(&record)? == plan.record.as_bytes(),
            "unexpected repair record contents"
        );
        actual_paths.insert(plan.record_path.clone());
    } else {
        ensure!(!final_state, "missing repair record");
    }
    expected_final.insert(
        plan.record_path.clone(),
        ("100644".into(), plan.record_oid.clone()),
    );
    let actual_index = index(&plan.root)?;
    ensure!(
        actual_index == expected_initial || actual_index == expected_final,
        "index differs from checkpoint or recorded repair"
    );
    let untracked = git(
        &plan.root,
        &["ls-files", "--others", "--exclude-standard", "-z"],
    )?;
    for path in untracked.split('\0').filter(|p| !p.is_empty()) {
        ensure!(
            actual_paths.contains(path),
            "unrelated untracked file: {path}"
        );
    }
    // Include ignored entries inside moved directories: a whole-directory
    // rename must never silently carry unknown attachments or symlinks.
    for (old, new) in &plan.mapping {
        for id in [old, new] {
            let dir = plan.root.join(".orgasmic/tasks").join(id);
            if present(&dir)? {
                verify_directory(&plan.root, &dir, &actual_paths)?;
            }
        }
    }
    if final_state {
        for new in plan.mapping.values() {
            let path = plan.root.join(".orgasmic/tasks").join(new).join("node.org");
            let file = OrgFile::parse(fs::read_to_string(&path)?, path.to_string_lossy())?;
            ensure!(file.headings.len() == 1, "invalid repaired node");
            let task = TaskHeading::from_heading(&file, &file.headings[0], "repaired node")?;
            ensure!(
                task.id == new && is_valid_task_path_id(task.id),
                "invalid repaired identity"
            );
        }
        for file in plan.files.iter().filter(|f| current_file(&f.target)) {
            let text = fs::read_to_string(plan.root.join(&file.target))?;
            ensure!(
                replace_refs(&text, &plan.mapping, &plan.project) == text,
                "malformed current reference remains in {}",
                file.target
            );
        }
    }
    Ok(())
}

fn verify_directory(root: &Path, dir: &Path, known: &BTreeSet<String>) -> Result<()> {
    safe_absolute(dir)?;
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        safe_absolute(&path)?;
        if path.is_dir() {
            let prefix = format!("{}/", path.strip_prefix(root)?.display());
            ensure!(
                known.iter().any(|p| p.starts_with(&prefix)),
                "unexpected task subdirectory {}",
                path.display()
            );
            verify_directory(root, &path, known)?;
        } else {
            ensure!(
                known.contains(path.strip_prefix(root)?.to_str().context("path UTF8")?),
                "unexpected task-directory content {}",
                path.display()
            );
        }
    }
    Ok(())
}

fn sync_dir(path: &Path) -> Result<()> {
    File::open(path)?.sync_all()?;
    Ok(())
}

fn atomic_write(path: &Path, bytes: &[u8], mode: Option<fs::Permissions>) -> Result<()> {
    let parent = path.parent().context("write parent")?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    if let Some(mode) = mode {
        temp.as_file().set_permissions(mode)?;
    }
    temp.write_all(bytes)?;
    temp.as_file().sync_all()?;
    temp.persist(path).map_err(|error| error.error)?;
    sync_dir(parent)
}

fn apply_plan(plan: &Plan, interrupt_after: Option<usize>) -> Result<()> {
    verify_state(plan, false)?;
    let mut operations = 0;
    for (old, new) in &plan.mapping {
        let parent = plan.root.join(".orgasmic/tasks");
        if parent.join(old).exists() {
            fs::rename(parent.join(old), parent.join(new))?;
            sync_dir(&parent)?;
        }
        operations += 1;
        ensure!(
            interrupt_after != Some(operations),
            "injected repair interruption"
        );
    }
    for file in &plan.files {
        if let Some(text) = &file.after {
            let path = plan.root.join(&file.target);
            if fs::read(&path)? != text.as_bytes() {
                atomic_write(
                    &path,
                    text.as_bytes(),
                    Some(fs::metadata(&path)?.permissions()),
                )?;
            }
            operations += 1;
            ensure!(
                interrupt_after != Some(operations),
                "injected repair interruption"
            );
        }
    }
    let record = plan.root.join(&plan.record_path);
    let parent = record.parent().context("record parent")?;
    if !parent.exists() {
        fs::create_dir(parent)?;
        sync_dir(parent.parent().context("record grandparent")?)?;
    }
    safe_absolute(parent)?;
    if !record.exists() {
        #[cfg(unix)]
        let mode = {
            use std::os::unix::fs::PermissionsExt;
            Some(fs::Permissions::from_mode(0o644))
        };
        #[cfg(not(unix))]
        let mode = None;
        atomic_write(&record, plan.record.as_bytes(), mode)?;
    }
    verify_state(plan, true)?;
    if git(&plan.root, &["rev-parse", "HEAD"])?.trim() == plan.checkpoint {
        let mut paths = BTreeSet::from([plan.record_path.as_str()]);
        for file in &plan.files {
            if file.source != file.target || file.after.is_some() {
                paths.insert(file.source.as_str());
                paths.insert(file.target.as_str());
            }
        }
        if git(&plan.root, &["diff", "--cached", "--name-only"])?.is_empty() {
            let mut args = vec!["-c", "core.fsync=all", "add", "-A", "--"];
            args.extend(paths.iter().copied());
            git(&plan.root, &args)?;
        }
        verify_state(plan, true)?;
        operations += 1;
        ensure!(
            interrupt_after != Some(operations),
            "injected repair interruption"
        );
        git(
            &plan.root,
            &[
                "-c",
                "core.fsync=all",
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-m",
                "repair: replace malformed terminal task IDs (TASK-ZZ2S4)",
            ],
        )?;
    }
    verify_state(plan, true)?;
    ensure!(
        git(
            &plan.root,
            &["status", "--porcelain=v1", "--untracked-files=all"]
        )?
        .is_empty(),
        "repair commit left dirty state"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use orgasmic_core::{claims::CLAIMED, TxEntry, TxWriter};

    fn fixture() -> (tempfile::TempDir, Home, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let home = Home::at(temp.path().canonicalize().unwrap().join("home"));
        home.ensure().unwrap();
        let root = home.project_ledger("fixture");
        fs::create_dir_all(&root).unwrap();
        git(&root, &["init", "-q"]).unwrap();
        git(&root, &["config", "user.name", "Repair test"]).unwrap();
        git(&root, &["config", "user.email", "repair@example.invalid"]).unwrap();
        write(
            &root,
            ".orgasmic/project.org",
            "#+orgasmic_version: 2\n* PROJECT fixture\n:PROPERTIES:\n:ID: fixture\n:END:\n",
        );
        write(&root, ".gitignore", "ignored\n");
        for id in ["TASK-CLM6W", "TASK-IXPD4", "TASK-LBRX7", "TASK-8DWJP"] {
            write(&root, &format!(".orgasmic/tasks/{id}/node.org"), &format!("#+title: orgasmic task {id}\n#+orgasmic_version: 2\n\n* DONE {id} Original title :tag:\n:PROPERTIES:\n:ID: {id}\n:PRIORITY: P1\n:DEPENDS_ON: TASK-CLM6W TASK-IXPD4\n:END:\n\n** Description\nKeep Unicode λ and spacing. See TASK-LBRX7.\n"));
            write(&root, &format!(".orgasmic/tasks/{id}/journal.org"), &format!("#+title: journal {id}\n* Historical event\n:PROPERTIES:\n:TASK: {id}\n:END:\nNever rewrite TASK-CLM6W\n"));
        }
        write(
            &root,
            ".orgasmic/tasks/goal.org",
            "Current [[TASK-CLM6W]] and fixture:TASK-IXPD4; other:TASK-LBRX7 stays.\n",
        );
        write(&root, ".orgasmic/decisions/dec_12345/node.org", "* Decision\n:PROPERTIES:\n:ID: dec_12345\n:IMPLEMENTS: TASK-IXPD4\n:END:\nTASK-LBRX7\n");
        write(
            &root,
            ".orgasmic/glossary/term_12345/node.org",
            "* Term\n:PROPERTIES:\n:ID: term_12345\n:END:\nTASK-CLM6W\n",
        );
        write(
            &root,
            ".orgasmic/machines/machine-a/tx.org",
            "Historical TASK-CLM6W event bytes\n",
        );
        commit(&root);
        projects::register_project(&home, &root, "fixture", "main").unwrap();
        (temp, home, root)
    }
    fn write(root: &Path, path: &str, text: &str) {
        let path = root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
    fn commit(root: &Path) {
        git(root, &["add", "-A"]).unwrap();
        git(
            root,
            &[
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-qm",
                "fixture",
            ],
        )
        .unwrap();
    }
    fn assert_history(plan: &Plan) {
        for file in plan.files.iter().filter(|f| !current_file(&f.source)) {
            assert_eq!(
                hash(&fs::read(plan.root.join(&file.target)).unwrap()),
                file.sha256,
                "{}",
                file.source
            );
        }
    }

    #[test]
    fn repair_task_ids_dry_run_apply_noop_and_exact_references() {
        let (_temp, home, root) = fixture();
        let before = git(&root, &["rev-parse", "HEAD"]).unwrap();
        run_at(&home, &root, false).unwrap();
        assert_eq!(git(&root, &["rev-parse", "HEAD"]).unwrap(), before);
        assert!(!home.task_id_repair_plan().exists());
        assert!(!home.root.join("daemon.lock").exists());
        assert!(git(&root, &["status", "--porcelain"]).unwrap().is_empty());
        let repair = plan(&home, &root).unwrap();
        assert_eq!(repair.mapping.len(), 3);
        atomic_write(
            &home.task_id_repair_plan(),
            &serde_json::to_vec(&repair).unwrap(),
            None,
        )
        .unwrap();
        run_at(&home, &root, true).unwrap();
        assert_history(&repair);
        assert!(!home.task_id_repair_plan().exists());
        let goal = fs::read_to_string(root.join(".orgasmic/tasks/goal.org")).unwrap();
        assert!(goal.contains("other:TASK-LBRX7"));
        assert!(goal.contains(&format!("fixture:{}", repair.mapping["TASK-IXPD4"])));
        let head = git(&root, &["rev-parse", "HEAD"]).unwrap();
        run_at(&home, &root, true).unwrap();
        assert_eq!(git(&root, &["rev-parse", "HEAD"]).unwrap(), head);
        let m = BTreeMap::from([("TASK-CLM6W".into(), "TASK-ABCDE".into())]);
        assert_eq!(replace_refs("TASK-CLM6W. TASK-CLM6W.1 TASK-CLM6WX xTASK-CLM6W other:TASK-CLM6W fixture:TASK-CLM6W", &m, "fixture"), "TASK-ABCDE. TASK-CLM6W.1 TASK-CLM6WX xTASK-CLM6W other:TASK-CLM6W fixture:TASK-ABCDE");
    }

    #[test]
    fn repair_task_ids_interrupted_rename_rewrite_and_commit_resume_safely() {
        for requested_stop in [1, 4, usize::MAX] {
            let (_temp, home, root) = fixture();
            let repair = plan(&home, &root).unwrap();
            atomic_write(
                &home.task_id_repair_plan(),
                &serde_json::to_vec(&repair).unwrap(),
                None,
            )
            .unwrap();
            let stop = if requested_stop == usize::MAX {
                repair.mapping.len() + repair.files.iter().filter(|f| f.after.is_some()).count() + 1
            } else {
                requested_stop
            };
            assert!(apply_plan(&repair, Some(stop))
                .unwrap_err()
                .to_string()
                .contains("interruption"));
            let runtime = tokio::runtime::Runtime::new().unwrap();
            let error = runtime
                .block_on(orgasmic_daemon::Daemon::run(
                    home.clone(),
                    orgasmic_daemon::DaemonOptions::default(),
                ))
                .err()
                .unwrap();
            assert!(error.to_string().contains("offline task-ID repair pending"));
            write(&root, "unrelated.txt", "do not lose me");
            assert!(run_at(&home, &root, true)
                .unwrap_err()
                .to_string()
                .contains("unrelated untracked"));
            assert_eq!(
                fs::read_to_string(root.join("unrelated.txt")).unwrap(),
                "do not lose me"
            );
            fs::remove_file(root.join("unrelated.txt")).unwrap();
            run_at(&home, &root, true).unwrap();
            assert_history(&repair);
            // A crash after commit but before unlink must also be resumable.
            atomic_write(
                &home.task_id_repair_plan(),
                &serde_json::to_vec(&repair).unwrap(),
                None,
            )
            .unwrap();
            let head = git(&root, &["rev-parse", "HEAD"]).unwrap();
            run_at(&home, &root, true).unwrap();
            assert_eq!(git(&root, &["rev-parse", "HEAD"]).unwrap(), head);
        }
    }

    #[test]
    fn repair_task_ids_refuses_dirty_lock_symlink_claim_descendant_and_edits() {
        let (_temp, home, root) = fixture();
        write(&root, "dirty", "keep");
        assert!(run_at(&home, &root, true)
            .unwrap_err()
            .to_string()
            .contains("dirty"));
        assert!(!home.task_id_repair_plan().exists());
        fs::remove_file(root.join("dirty")).unwrap();
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(home.root.join("daemon.lock"))
            .unwrap();
        fs2::FileExt::try_lock_exclusive(&lock).unwrap();
        assert!(run_at(&home, &root, true)
            .unwrap_err()
            .to_string()
            .contains("daemon is running"));
        drop(lock);
        let merge = root.join(
            git(&root, &["rev-parse", "--git-path", "MERGE_HEAD"])
                .unwrap()
                .trim(),
        );
        fs::write(&merge, git(&root, &["rev-parse", "HEAD"]).unwrap()).unwrap();
        assert!(run_at(&home, &root, true)
            .unwrap_err()
            .to_string()
            .contains("git operation in progress"));
        fs::remove_file(merge).unwrap();
        write(
            &root,
            ".orgasmic/tasks/TASK-CLM6W/ignored",
            "unknown attachment",
        );
        assert!(run_at(&home, &root, true)
            .unwrap_err()
            .to_string()
            .contains("unexpected task-directory content"));
        fs::remove_file(root.join(".orgasmic/tasks/TASK-CLM6W/ignored")).unwrap();
        #[cfg(unix)]
        {
            let path = root.join(".orgasmic/tasks/TASK-CLM6W/ignored");
            std::os::unix::fs::symlink("node.org", &path).unwrap();
            assert!(run_at(&home, &root, true)
                .unwrap_err()
                .to_string()
                .contains("symlink refused"));
            fs::remove_file(path).unwrap();
        }
        let claims = root.join(".orgasmic/machines/machine-a/claims.org");
        let mut event = TxEntry::new(
            "claim-a",
            CLAIMED,
            "[2026-09-06 Sun 10:00:00]",
            "test",
            "machine-a",
        );
        event.task = Some("TASK-CLM6W".into());
        TxWriter::open(&claims).unwrap().append(&event).unwrap();
        commit(&root);
        assert!(run_at(&home, &root, true)
            .unwrap_err()
            .to_string()
            .contains("active claim"));
        fs::remove_file(claims).unwrap();
        commit(&root);
        let child = ".orgasmic/tasks/TASK-12345/node.org";
        write(
            &root,
            child,
            "* DONE TASK-12345 Child\n:PROPERTIES:\n:ID: TASK-12345\n:PARENT: TASK-CLM6W\n:END:\n",
        );
        commit(&root);
        assert!(run_at(&home, &root, true)
            .unwrap_err()
            .to_string()
            .contains("descendants"));
        fs::remove_dir_all(root.join(".orgasmic/tasks/TASK-12345")).unwrap();
        commit(&root);
        let repair = plan(&home, &root).unwrap();
        atomic_write(
            &home.task_id_repair_plan(),
            &serde_json::to_vec(&repair).unwrap(),
            None,
        )
        .unwrap();
        assert!(apply_plan(&repair, Some(1)).is_err());
        let first = repair.mapping.values().next().unwrap();
        let path = root.join(".orgasmic/tasks").join(first).join("node.org");
        fs::write(&path, "unrelated edit").unwrap();
        assert!(run_at(&home, &root, true)
            .unwrap_err()
            .to_string()
            .contains("unexpected edit"));
        assert_eq!(fs::read_to_string(path).unwrap(), "unrelated edit");
        assert!(home.task_id_repair_plan().exists());
    }
}
