//! The runtime kit: what a generated bash function needs, described
//! once so a host, a container, a VM, and a remote box can all place
//! it. The file this module generates is static per stub set; the
//! values that differ per session are environment variables.
//!
//! See `specs/rhema-runtime-kit.md`.

use std::collections::BTreeMap;

/// The kit's on-disk contract. A helper whose `__kit-version` differs
/// is not used: the host and the init file must agree on the stub
/// shape.
/// The kit contract's version: the init file shape and the helper's
/// modes. Bumped when either changes, so an installed helper from an
/// older contract is replaced instead of silently missing a mode.
pub const KIT_VERSION: u32 = 3;

/// The generated surfaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Feature {
    /// Background shells: `job`.
    Jobs,
    /// Staged images: `see`.
    Vision,
    /// Web search: `search`.
    Search,
    /// Typed decision answers: `decide`.
    Decision,
}

impl Feature {
    /// The name the `__init` header and the debug notice carry.
    pub fn name(self) -> &'static str {
        match self {
            Self::Jobs => "jobs",
            Self::Vision => "vision",
            Self::Search => "search",
            Self::Decision => "decision",
        }
    }
}

/// What placement must supply for a kit to work.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Needs {
    /// An executable and an init file (every kit).
    pub exec: bool,
    /// State the host must read (jobs, vision): a shared root or a
    /// `read_file` pull.
    pub shared_state: bool,
    /// A live host endpoint, when a frontend wires one.
    pub bridge: bool,
    /// Host-side config or keys (search, decide).
    pub secrets: bool,
}

impl Needs {
    /// A macro over the helper and its per-session state: jobs and
    /// vision.
    pub const MACRO: Self = Self {
        exec: true,
        shared_state: true,
        bridge: false,
        secrets: false,
    };
    /// A macro whose CLI talks to host providers: search and decide.
    pub const HOST_CALL: Self = Self {
        exec: true,
        shared_state: false,
        bridge: false,
        secrets: true,
    };
}

/// One bash function: the name the model types, the env var naming the
/// helper, and the argv prefix the helper's mode reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stub {
    /// The function name, a bash identifier.
    pub name: String,
    /// The env var holding the helper path, a bash identifier.
    pub bin_var: String,
    /// Literal argv words between the helper and `"$@"`.
    pub args: Vec<String>,
}

impl Stub {
    /// Builds a stub, checking the two identifiers.
    pub fn new(
        name: impl Into<String>,
        bin_var: impl Into<String>,
        args: Vec<String>,
    ) -> Result<Self, String> {
        let stub = Self {
            name: name.into(),
            bin_var: bin_var.into(),
            args,
        };
        stub.validate()?;
        Ok(stub)
    }

    /// Parses the `__init --stub` form: `name=bin_var:args`, args
    /// whitespace-separated. The empty args tail is legal.
    pub fn parse(raw: &str) -> Result<Self, String> {
        let (name, rest) = raw
            .split_once('=')
            .ok_or_else(|| format!("stub `{raw}` is not name=bin_var:args"))?;
        let (bin_var, args) = rest
            .split_once(':')
            .ok_or_else(|| format!("stub `{raw}` is missing `:args`"))?;
        Self::new(
            name,
            bin_var,
            args.split_whitespace().map(str::to_owned).collect(),
        )
    }

    fn validate(&self) -> Result<(), String> {
        if !identifier(&self.name) {
            return Err(format!(
                "stub name `{}` is not a bash identifier",
                self.name
            ));
        }
        if !identifier(&self.bin_var) {
            return Err(format!(
                "stub bin var `{}` is not a bash identifier",
                self.bin_var
            ));
        }
        Ok(())
    }

    /// The `__init --stub` form, the inverse of [`Self::parse`].
    pub fn flag(&self) -> String {
        format!("{}={}:{}", self.name, self.bin_var, self.args.join(" "))
    }

    /// The function definition. A missing bin var is an in-band
    /// message, not an unbound-variable crash or a call to nothing:
    /// sessions share one init file, and a feature this session did
    /// not wire is absent, not broken.
    pub fn line(&self) -> String {
        let guard = format!(
            "if [ -z \"${{{bin_var}:-}}\" ]; then echo \"{name} is not available in this session\" >&2; return 1; fi; ",
            bin_var = self.bin_var,
            name = self.name
        );
        let tail = if self.args.is_empty() {
            String::new()
        } else {
            format!(
                "{} ",
                self.args
                    .iter()
                    .map(|arg| shquote(arg))
                    .collect::<Vec<_>>()
                    .join(" ")
            )
        };
        format!(
            "{name}() {{ {guard}\"${{{bin_var}}}\" {tail}\"$@\"; }}\n",
            name = self.name,
            bin_var = self.bin_var
        )
    }
}

/// One feature's description, no filesystem work. The wiring owns the
/// stubs and the vars; the kit owns the shape a reviewed placement
/// has to honor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Kit {
    /// Which surface this kit generates.
    pub feature: Feature,
    /// The functions the file defines.
    pub stubs: Vec<Stub>,
    /// Env entries beside `BASH_ENV` (token, profile, staging, ...).
    pub vars: BTreeMap<String, String>,
    /// What placement must supply for the kit to work.
    pub needs: Needs,
}

impl Kit {
    /// A kit with no extra vars.
    pub fn new(feature: Feature, stubs: Vec<Stub>, needs: Needs) -> Self {
        Self {
            feature,
            stubs,
            vars: BTreeMap::new(),
            needs,
        }
    }

    /// The env entries beside `BASH_ENV`.
    pub fn with_vars(mut self, vars: BTreeMap<String, String>) -> Self {
        self.vars = vars;
        self
    }
}

/// The header every generated init file starts with.
fn header() -> String {
    format!("# generated by rhema; runtime kit v{KIT_VERSION}; do not edit\n")
}

/// The init file for a set of stubs, in order. This is what the
/// helper's `__init` prints and what the placement's init run
/// redirects.
pub fn stubs_text(stubs: &[Stub]) -> String {
    let mut text = header();
    for stub in stubs {
        text.push_str(&stub.line());
    }
    text
}

/// The init file for a set of kits: the header and every stub, in the
/// order the kits were given. Static per kit version and stub set, so
/// one file serves every session with the same stubs.
pub fn init_text(kits: &[Kit]) -> String {
    let mut text = header();
    for kit in kits {
        for stub in &kit.stubs {
            text.push_str(&stub.line());
        }
    }
    text
}

/// Single-quotes `value` for bash, the one escape an argv word needs.
fn shquote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// `[A-Za-z_][A-Za-z0-9_]*`, the shape both a function name and an env
/// var need before they can ride a shell line.
fn identifier(value: &str) -> bool {
    let mut chars = value.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stub_line_guards_a_missing_bin_var() {
        let stub = Stub::new("job", "RHEMA_JOBS_BIN", vec!["__jobs".into()]).unwrap();
        let line = stub.line();
        assert!(line.contains("RHEMA_JOBS_BIN"), "{line}");
        assert!(
            line.contains("job is not available in this session"),
            "{line}"
        );
        assert!(
            line.contains("\"${RHEMA_JOBS_BIN}\" '__jobs' \"$@\""),
            "{line}"
        );
    }

    #[test]
    fn an_arg_with_a_quote_survives_as_one_word() {
        let stub = Stub::new(
            "mcp_x",
            "RHEMA_MCP_BIN",
            vec!["__server".into(), "a'b".into()],
        )
        .unwrap();
        let line = stub.line();
        assert!(line.contains("'__server' 'a'\\''b'"), "{line}");
    }

    #[test]
    fn a_stub_parses_the_flag_form() {
        let stub = Stub::parse("mcp_github=RHEMA_MCP_BIN:__server github").unwrap();
        assert_eq!(stub.name, "mcp_github");
        assert_eq!(stub.bin_var, "RHEMA_MCP_BIN");
        assert_eq!(stub.args, ["__server", "github"]);
        assert_eq!(stub.flag(), "mcp_github=RHEMA_MCP_BIN:__server github");
    }

    #[test]
    fn a_stub_rejects_a_non_identifier() {
        assert!(Stub::parse("rm -rf=RHEMA_BIN:__x").is_err());
        assert!(Stub::parse("job=RHEMA_BIN;rm:__x").is_err());
        assert!(Stub::parse("job").is_err());
    }

    #[test]
    fn the_init_text_is_static_per_kit_set() {
        let kit = Kit::new(
            Feature::Jobs,
            vec![Stub::new("job", "RHEMA_JOBS_BIN", vec!["__jobs".into()]).unwrap()],
            Needs::MACRO,
        );
        let text = init_text(std::slice::from_ref(&kit));
        assert!(
            text.starts_with(&format!("# generated by rhema; runtime kit v{KIT_VERSION}")),
            "{text}"
        );
        assert!(text.contains("job() {"), "{text}");
        assert_eq!(text, init_text(std::slice::from_ref(&kit)));
    }
}
