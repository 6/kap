/// Built-in ecosystem domain profiles.
/// Each profile is a curated set of domains needed by a specific ecosystem.

pub fn get(name: &str) -> Option<&'static [&'static str]> {
    match name {
        "ruby" => Some(RUBY),
        "node" => Some(NODE),
        "python" => Some(PYTHON),
        "rust" => Some(RUST),
        "go" => Some(GO),
        "apt" => Some(APT),
        "github" => Some(GITHUB),
        "ai" => Some(AI),
        _ => None,
    }
}

pub fn all_names() -> &'static [&'static str] {
    &[
        "ruby", "node", "python", "rust", "go", "apt", "github", "ai",
    ]
}

const RUBY: &[&str] = &[
    "rubygems.org",
    "*.rubygems.org",
    "bundler.io",
    "*.ruby-lang.org",
    "index.rubygems.org",
];

const NODE: &[&str] = &[
    "registry.npmjs.org",
    "*.npmjs.org",
    "*.npmjs.com",
    "nodejs.org",
    "*.yarnpkg.com",
];

const PYTHON: &[&str] = &[
    "pypi.org",
    "*.pypi.org",
    "files.pythonhosted.org",
    "*.pythonhosted.org",
];

const RUST: &[&str] = &[
    "crates.io",
    "*.crates.io",
    "static.crates.io",
    "rustup.rs",
    "*.rust-lang.org",
    "static.rust-lang.org",
];

const GO: &[&str] = &[
    "proxy.golang.org",
    "sum.golang.org",
    "storage.googleapis.com",
];

const APT: &[&str] = &[
    "*.ubuntu.com",
    "*.debian.org",
    "deb.debian.org",
    "security.debian.org",
    "archive.ubuntu.com",
    "security.ubuntu.com",
];

const GITHUB: &[&str] = &[
    "github.com",
    "api.github.com",
    "*.githubusercontent.com",
    "objects.githubusercontent.com",
    "raw.githubusercontent.com",
    "codeload.github.com",
];

const AI: &[&str] = &[
    "api.anthropic.com",
    "anthropic.com",
    "*.anthropic.com",
    "api.openai.com",
    "openai.com",
    "*.openai.com",
    "generativelanguage.googleapis.com",
];
