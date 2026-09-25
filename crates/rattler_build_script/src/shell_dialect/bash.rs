use std::fmt::Write as _;
use std::path::Path;

use indexmap::IndexMap;
use rattler_shell::shell::{self, Shell};

use super::{CommandSpec, ShellDialect};
use crate::{ExecutionContext, PrefixLayout};

pub(crate) struct BashDialect;

impl ShellDialect for BashDialect {
    fn shell(&self) -> shell::ShellEnum {
        shell::Bash::default().into()
    }

    fn default_interpreter(&self) -> &'static str {
        "bash"
    }

    fn preamble(&self, activation_script_path: Option<&Path>) -> String {
        let activation = activation_script_path
            .map(|path| {
                format!(
                    r#"if [ -z ${{CONDA_BUILD+x}} ]; then
    source "{}"
fi
"#,
                    path.to_string_lossy()
                )
            })
            .unwrap_or_default();
        format!(
            r#"#!/usr/bin/env bash
set -e
## Start of bash preamble
{activation}## End of preamble
# Trace each command as it runs so a failing line is visible (see #2264).
# Placed after activation so the sourced environment setup is not traced.
set -x
"#
        )
    }

    fn command_to_run_script(
        &self,
        build_script_path: &Path,
        _start_dir: &Path,
        _context: &ExecutionContext,
    ) -> CommandSpec {
        CommandSpec::new("bash", [build_script_path.to_string_lossy().into_owned()])
    }

    /// `$BASH` is the executable running the wrapper, so the child does not
    /// depend on the activated `PATH` to find a shell. The explicit `exit`
    /// stops the wrapper even when activation turned `set -e` off.
    fn child_script_command(&self, script_path: &Path, _context: &ExecutionContext) -> String {
        format!(
            "\"$BASH\" {} || exit $?\n",
            super::quote_arg(&self.shell(), &script_path.to_string_lossy())
        )
    }

    fn remove_files(&self, paths: &[&Path]) -> String {
        let shell = self.shell();
        let quoted = paths
            .iter()
            .map(|path| super::quote_arg(&shell, &path.to_string_lossy()))
            .collect::<Vec<_>>();
        format!("rm -f -- {} || exit 1\n", quoted.join(" "))
    }

    /// `cmp -s` compares the files byte by byte, NUL bytes included, which
    /// a comparison through command substitution would drop. Only paths
    /// appear on the traced lines, never what the files contain, so the
    /// tracing state activation left is kept as it is.
    fn check_step_manifest(
        &self,
        manifest: &Path,
        recorded: Option<&Path>,
        message: &str,
    ) -> String {
        let shell = self.shell();
        let quote = |path: &Path| super::quote_arg(&shell, &path.to_string_lossy());
        let manifest = quote(manifest);
        let condition = match recorded {
            None => format!("[ -s {manifest} ]"),
            Some(recorded) => {
                let recorded = quote(recorded);
                format!(
                    "[ ! -f {manifest} ] || [ ! -f {recorded} ] || ! cmp -s {manifest} {recorded}"
                )
            }
        };
        format!(
            "if {condition}; then\n    \
             printf '%s\\n' {} >&2\n    \
             exit 1\n\
             fi\n",
            super::quote_arg(&shell, message)
        )
    }

    fn replacements_template(&self) -> &'static str {
        "$((var))"
    }

    /// Subshell scope. Inherited `set -e` aborts on failure, so no guard is
    /// needed — but it must stay a bare statement (chaining `||`/`&&` would
    /// suppress `set -e`).
    fn scope_section(
        &self,
        label: Option<&str>,
        env: &IndexMap<String, String>,
        literal_env: &[(&str, &str)],
        cwd: Option<&Path>,
        body: &str,
    ) -> Result<String, std::io::Error> {
        let shell = shell::Bash::default();
        let mut out = String::new();
        if let Some(label) = label {
            let _ = writeln!(out, "# === {label} ===");
        }
        out.push_str("(\n");
        for (key, value) in env {
            super::validate_env_assignment(key, value)?;
            shell
                .set_env_var(&mut out, key, value)
                .map_err(std::io::Error::other)?;
        }
        for (name, value) in literal_env {
            self.set_literal_env_var(&mut out, name, value)?;
        }
        if let Some(cwd) = cwd {
            let cwd = super::quote_arg(&self.shell(), &cwd.to_string_lossy());
            let _ = writeln!(out, "cd {cwd}");
        }
        out.push_str(body);
        if !body.ends_with('\n') {
            out.push('\n');
        }
        out.push(')');
        Ok(out)
    }

    /// `value` is single-quoted (see [`super::quote_arg`]) unless it has only
    /// characters bash never interprets: single quotes keep every character
    /// literal, and a single quote in `value` is written as `'\''`.
    /// [`Shell::set_env_var`] instead double-quotes a value containing `$`,
    /// in which `$`, command substitutions, and backticks are expanded.
    fn set_literal_env_var(
        &self,
        out: &mut String,
        name: &str,
        value: &str,
    ) -> Result<(), std::io::Error> {
        super::validate_env_assignment(name, value)?;
        let _ = writeln!(
            out,
            "export {name}={}",
            super::quote_arg(&self.shell(), value)
        );
        Ok(())
    }

    /// Returns reproduction instructions for the failed bash wrapper script.
    fn debug_info(&self, work_dir: &Path, context: &ExecutionContext) -> String {
        let mut output = String::new();

        output.push_str("\nScript execution failed.\n\n");
        output.push_str(&format!("  Work directory: {}\n", work_dir.display()));
        output.push_str(&format!("  Prefix: {}\n", context.host().path().display()));

        if context.layout() == PrefixLayout::Separate {
            output.push_str(&format!(
                "  Build prefix: {}\n",
                context.build().path().display()
            ));
        } else {
            output.push_str("  Build prefix: None\n");
        }

        output.push_str("\nTo run the script manually, use the following command:\n\n");
        output.push_str(&format!("  cd {:?} && ./conda_build.sh\n\n", work_dir));
        output.push_str("To run commands interactively in the build environment:\n\n");
        output.push_str(&format!("  cd {:?} && source build_env.sh", work_dir));

        output
    }
}
