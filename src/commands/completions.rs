//! `runway completions SHELL`: shell completion scripts.
//!
//! bash, zsh, fish, elvish and PowerShell come from `clap_complete`, Nushell
//! from `clap_complete_nushell`. Xonsh has no clap generator: runway emits a
//! native xonsh completer built from the same command definition.

use crate::cli::{Cli, CompletionsArgs, Shell};
use crate::error::Result;
use clap::CommandFactory;
use std::collections::BTreeMap;
use std::io::Write;

pub fn run(args: CompletionsArgs) -> Result<()> {
    let mut cmd = Cli::command();
    let mut out = std::io::stdout().lock();
    let name = "runway";
    match args.shell {
        Shell::Bash => {
            clap_complete::generate(clap_complete::Shell::Bash, &mut cmd, name, &mut out)
        }
        Shell::Zsh => clap_complete::generate(clap_complete::Shell::Zsh, &mut cmd, name, &mut out),
        Shell::Fish => {
            clap_complete::generate(clap_complete::Shell::Fish, &mut cmd, name, &mut out)
        }
        Shell::Elvish => {
            clap_complete::generate(clap_complete::Shell::Elvish, &mut cmd, name, &mut out)
        }
        Shell::Powershell => {
            clap_complete::generate(clap_complete::Shell::PowerShell, &mut cmd, name, &mut out)
        }
        Shell::Nushell => {
            clap_complete::generate(clap_complete_nushell::Nushell, &mut cmd, name, &mut out)
        }
        Shell::Xonsh => {
            out.write_all(xonsh_script(&cmd).as_bytes())?;
        }
    }
    Ok(())
}

/// Flags (`--long` and `-s`) of a command, and the values of enum options.
fn flags(cmd: &clap::Command, values: &mut BTreeMap<String, Vec<String>>) -> Vec<String> {
    let mut out = Vec::new();
    for a in cmd.get_arguments().filter(|a| !a.is_positional()) {
        let mut names = Vec::new();
        if let Some(l) = a.get_long() {
            names.push(format!("--{l}"));
        }
        if let Some(s) = a.get_short() {
            names.push(format!("-{s}"));
        }
        let pv: Vec<String> = a
            .get_possible_values()
            .iter()
            .filter(|v| !v.is_hide_set())
            .map(|v| v.get_name().to_string())
            .collect();
        if !pv.is_empty() {
            for n in &names {
                values.insert(n.clone(), pv.clone());
            }
        }
        out.extend(names);
    }
    out.push("--help".into());
    out.sort();
    out.dedup();
    out
}

fn py_list(items: &[String]) -> String {
    let quoted: Vec<String> = items.iter().map(|s| format!("{s:?}")).collect();
    format!("[{}]", quoted.join(", "))
}

pub fn xonsh_script(cmd: &clap::Command) -> String {
    let mut values = BTreeMap::new();
    let global = flags(cmd, &mut values);
    let mut subs = BTreeMap::new();
    let mut positional = BTreeMap::new();
    for sc in cmd.get_subcommands().filter(|s| !s.is_hide_set()) {
        subs.insert(sc.get_name().to_string(), flags(sc, &mut values));
        let pv: Vec<String> = sc
            .get_positionals()
            .flat_map(|a| a.get_possible_values())
            .map(|v| v.get_name().to_string())
            .collect();
        if !pv.is_empty() {
            positional.insert(sc.get_name().to_string(), pv);
        }
    }
    let positional_py: Vec<String> = positional
        .iter()
        .map(|(k, v)| format!("    {k:?}: {},", py_list(v)))
        .collect();
    let subs_py: Vec<String> = subs
        .iter()
        .map(|(k, v)| format!("    {k:?}: {},", py_list(v)))
        .collect();
    let values_py: Vec<String> = values
        .iter()
        .map(|(k, v)| format!("    {k:?}: {},", py_list(v)))
        .collect();
    format!(
        r#"# runway completions for xonsh. Load with:  source-auto (runway completions xonsh)
# or save to a file and `source` it from ~/.xonshrc.
from xonsh.completers.tools import contextual_command_completer

_RUNWAY_GLOBAL = {global}
_RUNWAY_SUBCOMMANDS = {{
{subs}
}}
_RUNWAY_VALUES = {{
{values}
}}
_RUNWAY_POSITIONAL = {{
{positional}
}}


@contextual_command_completer
def _runway_completer(ctx):
    """Subcommands, flags and enum values of `runway`."""
    if ctx.command != "runway":
        return None
    words = [a.value for a in ctx.args[1 : ctx.arg_index]]
    prefix = ctx.prefix
    if words and words[-1] in _RUNWAY_VALUES:
        return {{v for v in _RUNWAY_VALUES[words[-1]] if v.startswith(prefix)}}
    sub = next((w for w in words if w in _RUNWAY_SUBCOMMANDS), None)
    if sub is None and not prefix.startswith("-"):
        return {{s for s in _RUNWAY_SUBCOMMANDS if s.startswith(prefix)}}
    if sub in _RUNWAY_POSITIONAL and not prefix.startswith("-"):
        return {{v for v in _RUNWAY_POSITIONAL[sub] if v.startswith(prefix)}}
    opts = list(_RUNWAY_GLOBAL) + (_RUNWAY_SUBCOMMANDS[sub] if sub else [])
    return {{o for o in opts if o.startswith(prefix)}}


try:
    from xonsh.completers.completer import add_one_completer

    add_one_completer("runway", _runway_completer, "start")
except ImportError:
    completer add runway _runway_completer "start"
"#,
        global = py_list(&global),
        subs = subs_py.join("\n"),
        values = values_py.join("\n"),
        positional = positional_py.join("\n"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xonsh_script_lists_subcommands_flags_and_values() {
        let s = xonsh_script(&Cli::command());
        assert!(s.contains(r#""deploy": ["#));
        assert!(s.contains(r#""--preview""#));
        assert!(s.contains(r#""traffic": ["#));
        assert!(
            s.contains(r#""--color": ["auto", "always", "never"]"#),
            "{s}"
        );
        assert!(s.contains("add_one_completer(\"runway\""));
        assert!(
            s.contains(r#""completions": ["bash", "zsh", "fish", "nushell", "xonsh""#),
            "{s}"
        );
    }
}
