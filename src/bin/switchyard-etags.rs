//! Generate an emacs `TAGS` file for switchyard's Lisp config.
//!
//! Walks every entry-point file plus everything each transitively
//! `(load …)`s, recording each `(defun NAME …)` location. Drop the
//! resulting `TAGS` next to the sources and `M-.` /
//! `xref-find-definitions` jumps from a call site to its definition.
//! tulisp's builtin defuns get tagged too — `M-.` into `if-let`,
//! `when`, etc. lands in tulisp's own Rust source.
//!
//! Usage:
//!
//!   switchyard-etags                              # ./config.lisp → ./TAGS
//!   switchyard-etags config.lisp scenarios/example.lisp
//!   switchyard-etags config.lisp -o /tmp/TAGS

use std::{env, io::Write as _, path::Path, process};

use switchyard::lisp::Config;

fn main() {
    let mut args = env::args().skip(1);
    let mut roots: Vec<String> = Vec::new();
    let mut output_path: String = "TAGS".to_string();

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-o" => {
                output_path = args.next().unwrap_or_else(|| {
                    eprintln!("error: -o needs an output path");
                    process::exit(2);
                });
            }
            "-h" | "--help" => {
                eprintln!(
                    "usage: switchyard-etags [FILE …] [-o OUTPUT]\n\
                     Default FILE = config.lisp; default OUTPUT = TAGS",
                );
                process::exit(0);
            }
            // emacs's etags-regen appends a trailing `-` (the
            // etags(1) stdin-marker convention). Skip it. Any
            // non-.lisp positional arg gets dropped too — emacs
            // discovers files via etags-regen-file-extensions and
            // we don't care about Rust / elisp paths here.
            "-" => continue,
            other if !other.ends_with(".lisp") => continue,
            other => roots.push(other.to_string()),
        }
    }

    if roots.is_empty() {
        roots.push("config.lisp".to_string());
    }

    for r in &roots {
        if !Path::new(r).exists() {
            eprintln!("error: file not found: {r}");
            process::exit(1);
        }
    }

    let root_refs: Vec<&str> = roots.iter().map(String::as_str).collect();
    let table = Config::tags_table(root_refs.as_slice()).unwrap_or_else(|e| {
        eprintln!("error: tags_table({roots:?}): {e:?}");
        process::exit(1);
    });

    let mut file = std::fs::File::create(&output_path).unwrap_or_else(|e| {
        eprintln!("error: create({output_path}): {e}");
        process::exit(1);
    });
    file.write_all(table.as_bytes()).unwrap_or_else(|e| {
        eprintln!("error: write({output_path}): {e}");
        process::exit(1);
    });
    println!("wrote {output_path} ({} root file(s))", roots.len());
}
