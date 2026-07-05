//! Host-local member management (`orgasmic member add|revoke|list`).
//!
//! Thin CLI plumbing over `orgasmic_core::{add_member, revoke_member,
//! read_members}`. Member records live at `$ORGASMIC_HOME/user/auth/members.org`
//! and the daemon re-reads that file fresh on every request, so a write here is
//! immediately effective — no daemon poke or restart. Member management is
//! intentionally host-local (admin by virtue of filesystem access) and is not
//! an HTTP route.

use anyhow::{bail, Result};
use clap::{Subcommand, ValueEnum};

use crate::home::Home;

/// The three built-in roles v1 mints. Roles are free-form in the data model,
/// but the CLI stays strict so a typo can't quietly create a capability-less
/// grant.
const BUILTIN_ROLES: [&str; 3] = ["editor", "viewer", "artifacts"];

#[derive(ValueEnum, Clone, Copy, Debug)]
pub(crate) enum RoleArg {
    Editor,
    Viewer,
    Artifacts,
}

impl RoleArg {
    fn as_str(self) -> &'static str {
        match self {
            RoleArg::Editor => "editor",
            RoleArg::Viewer => "viewer",
            RoleArg::Artifacts => "artifacts",
        }
    }
}

#[derive(Subcommand, Debug)]
pub enum MemberCmd {
    /// Mint a member token with capability grants (prints the token once).
    #[command(after_help = "\
Examples:
  orgasmic member add alice --role editor
  orgasmic member add bob --grant proj-a=editor --grant proj-b=viewer
  orgasmic member add carol --role viewer --grant proj-a=editor

--role R seeds a wildcard grant (*=R) applying to every project. Each --grant
PROJECT=ROLE adds a project-scoped grant. At least one of --role/--grant is
required. ROLE is one of: editor, viewer, artifacts.")]
    Add {
        /// Member name ([A-Za-z0-9_.-], 1-64 chars).
        name: String,
        /// Seed a wildcard grant (`*=ROLE`) that applies to every project.
        #[arg(long, value_enum)]
        role: Option<RoleArg>,
        /// Project-scoped grant `PROJECT=ROLE`; repeatable.
        #[arg(long = "grant", value_name = "PROJECT=ROLE")]
        grant: Vec<String>,
    },
    /// Revoke a member; all sessions from that token become invalid.
    Revoke {
        /// Member name to revoke.
        name: String,
    },
    /// List members and their grants (never prints a full secret).
    List,
}

pub fn cmd_member(home: &Home, cmd: MemberCmd) -> Result<()> {
    match cmd {
        MemberCmd::Add { name, role, grant } => cmd_add(home, &name, role, &grant),
        MemberCmd::Revoke { name } => cmd_revoke(home, &name),
        MemberCmd::List => cmd_list(home),
    }
}

fn cmd_add(home: &Home, name: &str, role: Option<RoleArg>, grant: &[String]) -> Result<()> {
    let grants = build_grants(role, grant)?;
    let token = orgasmic_core::add_member(home, name, &grants)?;

    println!("✓ minted member {name}");
    println!("  grants: {}", format_grants(&grants));
    println!();
    println!("  token (shown only once — store it now, it is not recoverable):");
    println!();
    println!("    {token}");
    println!();
    println!("  Give this token to {name}; they log in with it via the connect screen.");
    Ok(())
}

fn cmd_revoke(home: &Home, name: &str) -> Result<()> {
    if orgasmic_core::revoke_member(home, name)? {
        println!("✓ revoked member {name}");
        println!("  all sessions from that token are now invalid.");
    } else {
        println!("no member named {name} on file (nothing to revoke)");
    }
    Ok(())
}

fn cmd_list(home: &Home) -> Result<()> {
    let members = orgasmic_core::read_members(home)?;
    if members.is_empty() {
        println!("(no members — add one with `orgasmic member add <name> --role <role>`)");
        return Ok(());
    }
    for m in &members {
        let hash_prefix: String = m.token_hash.chars().take(12).collect();
        println!(
            "{}  [{}…]  {}",
            m.name,
            hash_prefix,
            format_grants(&m.grants)
        );
    }
    Ok(())
}

/// Combine a `--role` wildcard seed with any `--grant PROJECT=ROLE` pairs into
/// the `(project-or-*, role)` grant list the core expects. Errors when neither
/// is given, a `--grant` value has no `=`, or a role is not one of the three
/// built-ins.
fn build_grants(role: Option<RoleArg>, grants: &[String]) -> Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    if let Some(role) = role {
        out.push(("*".to_string(), role.as_str().to_string()));
    }
    for raw in grants {
        let (project, role) = raw.split_once('=').ok_or_else(|| {
            anyhow::anyhow!("--grant expects PROJECT=ROLE, got `{raw}` (missing `=`)")
        })?;
        if project.is_empty() {
            bail!("--grant project must not be empty in `{raw}`");
        }
        validate_role(role)?;
        out.push((project.to_string(), role.to_string()));
    }
    if out.is_empty() {
        bail!("member add requires at least one of --role or --grant");
    }
    Ok(out)
}

fn validate_role(role: &str) -> Result<()> {
    if !BUILTIN_ROLES.contains(&role) {
        bail!(
            "unknown role `{role}`: the CLI only mints one of {}",
            BUILTIN_ROLES.join(", ")
        );
    }
    Ok(())
}

fn format_grants(grants: &[(String, String)]) -> String {
    grants
        .iter()
        .map(|(project, role)| format!("{project}={role}"))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_seeds_wildcard_grant() {
        let grants = build_grants(Some(RoleArg::Editor), &[]).unwrap();
        assert_eq!(grants, vec![("*".to_string(), "editor".to_string())]);
    }

    #[test]
    fn role_and_grant_combine_in_order() {
        let grants =
            build_grants(Some(RoleArg::Viewer), &["proj-a=editor".to_string()]).unwrap();
        assert_eq!(
            grants,
            vec![
                ("*".to_string(), "viewer".to_string()),
                ("proj-a".to_string(), "editor".to_string()),
            ]
        );
    }

    #[test]
    fn grant_without_equals_errors() {
        let err = build_grants(None, &["foo".to_string()]).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("PROJECT=ROLE"), "got: {msg}");
        assert!(msg.contains("missing `=`"), "got: {msg}");
    }

    #[test]
    fn grant_with_empty_project_errors() {
        let err = build_grants(None, &["=editor".to_string()]).unwrap_err();
        assert!(format!("{err}").contains("must not be empty"));
    }

    #[test]
    fn unknown_grant_role_errors() {
        let err = build_grants(None, &["proj-a=admin".to_string()]).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("unknown role"), "got: {msg}");
        assert!(msg.contains("admin"), "got: {msg}");
    }

    #[test]
    fn neither_role_nor_grant_errors() {
        let err = build_grants(None, &[]).unwrap_err();
        assert!(format!("{err}").contains("at least one of --role or --grant"));
    }

    #[test]
    fn multiple_grants_preserve_first_equals_split() {
        // Only the first `=` splits; the rest is part of the role and is
        // rejected by role validation (a value with `=` is never a built-in).
        let err = build_grants(None, &["proj=a=editor".to_string()]).unwrap_err();
        assert!(format!("{err}").contains("unknown role"));
    }
}
