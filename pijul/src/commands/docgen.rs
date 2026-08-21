use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use clap::{Arg, Command, CommandFactory, Parser};

use crate::Opts;

/// Generate documentation (man pages or markdown) from the command definitions.
///
/// This command is intended for use by the build system and website tooling,
/// not for end users.
#[derive(Parser, Debug)]
#[clap(hide = true)]
pub struct Docgen {
    #[clap(subcommand)]
    format: Format,
}

#[derive(Parser, Debug)]
enum Format {
    /// Generate man pages (one per subcommand) into <outdir>
    Man {
        /// Directory to write .1 files into
        outdir: PathBuf,
    },
    /// Generate markdown reference pages (one per subcommand) into <outdir>
    Markdown {
        /// Directory to write .md files into
        outdir: PathBuf,
    },
}

impl Docgen {
    pub fn repository_path(&self) -> Option<&Path> {
        None
    }

    pub fn run(self) -> Result<(), anyhow::Error> {
        let app = Opts::command();
        match self.format {
            Format::Man { outdir } => {
                fs::create_dir_all(&outdir)?;
                gen_man(&app, &outdir)?;
            }
            Format::Markdown { outdir } => {
                fs::create_dir_all(&outdir)?;
                gen_markdown(&app, &outdir)?;
            }
        }
        Ok(())
    }
}

fn gen_man(app: &Command, outdir: &Path) -> Result<(), anyhow::Error> {
    let man = clap_mangen::Man::new(app.clone());
    let mut buf = Vec::new();
    man.render(&mut buf)?;
    let path = outdir.join(format!("{}.1", app.get_name()));
    fs::write(&path, &buf)?;

    for sub in app.get_subcommands() {
        if sub.is_hide_set() {
            continue;
        }
        let man = clap_mangen::Man::new(sub.clone());
        let mut buf = Vec::new();
        man.render(&mut buf)?;
        let name = format!("pijul-{}.1", sub.get_name());
        fs::write(outdir.join(name), &buf)?;
    }
    Ok(())
}

fn gen_markdown(app: &Command, outdir: &Path) -> Result<(), anyhow::Error> {
    // Top-level index page listing all subcommands
    let mut index = Vec::new();
    writeln!(index, "# pijul")?;
    writeln!(index)?;
    if let Some(about) = app.get_about() {
        writeln!(index, "{}", about)?;
        writeln!(index)?;
    }
    writeln!(index, "## Subcommands")?;
    writeln!(index)?;

    for sub in app.get_subcommands() {
        if sub.is_hide_set() {
            continue;
        }
        let name = sub.get_name();
        let desc = sub.get_about().map(|s| s.to_string()).unwrap_or_default();
        writeln!(index, "- [`pijul {name}`]({name}.md) — {desc}")?;

        let path = outdir.join(format!("{}.md", name));
        let mut f = fs::File::create(&path)?;
        render_subcommand(&mut f, app.get_name(), sub)?;
    }

    fs::write(outdir.join("index.md"), &index)?;
    Ok(())
}

fn render_subcommand(w: &mut impl Write, parent: &str, cmd: &Command) -> Result<(), anyhow::Error> {
    let name = cmd.get_name();
    let full_name = format!("{parent} {name}");

    writeln!(w, "# {full_name}")?;
    writeln!(w)?;

    if let Some(about) = cmd.get_long_about().or_else(|| cmd.get_about()) {
        writeln!(w, "{about}")?;
        writeln!(w)?;
    }

    // Usage line
    writeln!(w, "## Usage")?;
    writeln!(w)?;
    write!(w, "```")?;
    write!(w, "\n{full_name}")?;
    if cmd.get_subcommands().next().is_some() {
        write!(w, " <SUBCOMMAND>")?;
    }
    for arg in cmd.get_positionals() {
        write!(w, " {}", usage_metavar(arg))?;
    }
    if cmd.get_opts().next().is_some() {
        write!(w, " [OPTIONS]")?;
    }
    writeln!(w, "\n```")?;
    writeln!(w)?;

    // Subcommands
    let subs: Vec<_> = cmd.get_subcommands().filter(|s| !s.is_hide_set()).collect();
    if !subs.is_empty() {
        writeln!(w, "## Subcommands")?;
        writeln!(w)?;
        for sub in &subs {
            let desc = sub.get_about().map(|s| s.to_string()).unwrap_or_default();
            writeln!(w, "- `{}` — {}", sub.get_name(), desc)?;
        }
        writeln!(w)?;
    }

    // Arguments
    let args: Vec<_> = cmd
        .get_arguments()
        .filter(|a| !a.is_hide_set() && !a.is_global_set())
        .collect();
    if !args.is_empty() {
        writeln!(w, "## Options")?;
        writeln!(w)?;
        for arg in &args {
            write!(w, "- ")?;
            let mut names = Vec::new();
            if let Some(short) = arg.get_short() {
                names.push(format!("`-{short}`"));
            }
            if let Some(long) = arg.get_long() {
                names.push(format!("`--{long}`"));
            }
            if names.is_empty() {
                names.push(format!("`{}`", arg.get_id()));
            }
            write!(w, "{}", names.join(", "))?;
            if let Some(help) = arg.get_long_help().or_else(|| arg.get_help()) {
                write!(w, " — {help}")?;
            }
            writeln!(w)?;
        }
        writeln!(w)?;
    }

    Ok(())
}

fn usage_metavar(arg: &Arg) -> String {
    let id = arg.get_id().as_str();
    if arg
        .get_num_args()
        .map(|r| r.min_values() == 0)
        .unwrap_or(false)
    {
        format!("[{id}]")
    } else {
        format!("<{id}>")
    }
}
