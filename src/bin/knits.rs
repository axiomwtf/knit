// knits - the Knits remote side. Stores bare yarnballs and serves them
// over stdio so it can be exec'd by ssh: `ssh host knits receive repo`.

use std::fs;
use std::io::{Read, Write};
use std::path::PathBuf;

fn usage() -> ! {
    eprintln!("usage: knits [--dir DIR] <command>");
    eprintln!("commands:");
    eprintln!("  receive <name>   read a .knit stream from stdin, store atomically");
    eprintln!("  cat <name>       write a stored yarnball to stdout");
    eprintln!("  ls               list stored yarnballs");
    eprintln!();
    eprintln!("dir: --dir flag, KNITS_DIR env, or ~/.knits");
    std::process::exit(2);
}

fn dirs_home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut dir = None;
    let mut positional: Vec<&String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--dir" {
            dir = args.get(i + 1).map(PathBuf::from);
            i += 2;
        } else {
            positional.push(&args[i]);
            i += 1;
        }
    }
    let dir = dir.unwrap_or_else(|| {
        std::env::var("KNITS_DIR").map(PathBuf::from).unwrap_or_else(|_| dirs_home().join(".knits"))
    });

    match positional.first().map(|s| s.as_str()) {
        Some("receive") => {
            let name = positional.get(1).context("receive needs a name")?;
            fs::create_dir_all(&dir)?;
            let dest = dir.join(format!("{name}.knit"));
            let tmp = dir.join(format!("{name}.knit.tmp"));
            let mut input = Vec::new();
            std::io::stdin().read_to_end(&mut input)?;
            if input.len() < 32 || &input[..4] != b"KNIT" {
                anyhow::bail!("stdin is not a .knit stream");
            }
            fs::write(&tmp, &input)?;
            fs::rename(&tmp, &dest)?;
        }
        Some("cat") => {
            let name = positional.get(1).context("cat needs a name")?;
            let p = dir.join(format!("{name}.knit"));
            let data = fs::read(&p)?;
            std::io::stdout().write_all(&data)?;
        }
        Some("ls") => {
            if dir.is_dir() {
                for e in fs::read_dir(&dir)? {
                    let p = e?.path();
                    if p.extension().map(|x| x == "knit").unwrap_or(false) {
                        println!("{}", p.file_stem().unwrap().to_string_lossy());
                    }
                }
            }
        }
        _ => usage(),
    }
    Ok(())
}

use anyhow::Context;
