// Build helper for Windows hosts.
//
// Recent `windows-sys` crates link with raw-dylib, which makes rustc call GNU
// `dlltool`, and that needs an assembler the Rust GNU toolchain does not ship.
// This shim accepts rustc's dlltool arguments and forwards them to
// `zig dlltool` (llvm-dlltool), which needs no assembler.
// Zig is taken from $ZIG, or from `python -m ziglang` (pip install ziglang).

use std::process::{exit, Command};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut forward: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-d" | "-D" | "-l" | "-m" => {
                forward.push(args[i].clone());
                if let Some(v) = args.get(i + 1) {
                    forward.push(v.clone());
                }
                i += 2;
            }
            // GNU-only options (assembler flags, temp prefix) are not needed.
            "-f" | "--temp-prefix" | "--as" => i += 2,
            _ => i += 1,
        }
    }
    let mut cmd = match std::env::var("ZIG") {
        Ok(zig) => Command::new(zig),
        Err(_) => {
            let mut c = Command::new("python");
            c.args(["-m", "ziglang"]);
            c
        }
    };
    match cmd.arg("dlltool").args(&forward).status() {
        Ok(s) => exit(s.code().unwrap_or(1)),
        Err(e) => {
            eprintln!("dlltool shim: cannot run zig: {e}");
            exit(1);
        }
    }
}
