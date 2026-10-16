# diff-probe

**Inspect a unified diff against your files without applying it.**

`diff-probe` is a small, read-only command-line tool that shows what a patch expects to find in the original files. It locates each hunk's preimage, displays nearby source lines and a preview of proposed additions, and highlights differences in content, position, or line endings.

- **Position-aware:** distinguish exact, shifted, and ambiguous matches.
- **Line-ending-aware:** distinguish LF, CRLF, missing final LF, and unknown original line endings.
- **Readable reports:** see changes in source context, with optional terminal colors.
- **Lightweight:** Rust 2024, standard library only, no third-party crates.

## Platforms

| Operating system | Architecture | Rust target |
| --- | --- | --- |
| Windows 10 / 11 | x64 | `x86_64-pc-windows-msvc` |
| Linux (glibc) | x64 | `x86_64-unknown-linux-gnu` |
| macOS | Apple Silicon (ARM64) | `aarch64-apple-darwin` |

## Build and use

Build from source with stable Rust:

```sh
cargo build --release --locked
```

Inspect `change.patch` against files in the current directory.

**Windows (PowerShell):**

```powershell
.\target\release\diff-probe.exe .\change.patch --root .
```

**Linux / macOS:**

```sh
./target/release/diff-probe ./change.patch --root .
```

Show more surrounding lines or disable color when saving the report:

```sh
diff-probe change.patch --root . --context 12 --color never
```

The last example assumes `diff-probe` is on your `PATH`. `--root` identifies the directory containing the files referenced by the patch; it defaults to the current directory.

## Command-line options

```text
Usage: diff-probe PATCH [--root DIR] [--context N] [--color auto|always|never]
```

| Option | Description |
| --- | --- |
| `PATCH` | Unified-diff file to inspect (required). |
| `--root DIR` | Directory containing the original files (default: `.`). |
| `--context N` | Surrounding source lines, from `0` to `1000` (default: `5`). |
| `--color MODE` | `auto`, `always`, or `never` (default: `auto`; respects `NO_COLOR` and redirection). |
| `--help` | Print usage. |

## Understanding the report

`diff-probe` prints the original source around each hunk and places up to eight proposed `+` lines per hunk where they would appear. The main results are:

| Result | Meaning |
| --- | --- |
| `MATCH` | The preimage matches exactly at its expected position, including line endings. |
| `SHIFTED` | An exact preimage exists at another position. |
| `AMBIGUOUS` | Multiple possible matching positions were found. |
| `CONTENT ONLY` | Text matches, but line endings differ. |
| `CONTENT MATCH` | Text matches, but the original line endings cannot be determined. |
| `INSERTION` | A pure insertion has no original lines to verify. |
| `NOT FOUND` / `RANGE OUTSIDE FILE` | The expected preimage is absent or extends beyond the file. |
| `UNDETERMINED` | The search budget was exhausted. |

Report prefixes identify hunk context (`=`), removals (`-`), additions (`+`), insertion points (`>`), and mismatches (`!`). Surrounding source lines have no prefix. The report also shows line-ending counts for inspected files.

### Exit codes

| Code | Meaning |
| --- | --- |
| `0` | No mismatch or unverifiable section was detected in the preimage comparison. |
| `1` | At least one section cannot be verified as an exact preimage match. |
| `2` | Invalid input, a filesystem or I/O error, or a resource limit was reached. |

Reports go to stdout and errors to stderr. An error can leave an incomplete report on stdout; check the exit code before using the output.

## Scope

`diff-probe` understands unified hunks produced by tools such as `git diff`, `diff -u`, `git show`, and `git format-patch`. Diff-header paths must be valid UTF-8.

It checks **preimages**, not full patch applicability. It does not simulate earlier changes or verify Git operations such as renames, copies, file modes, or binary modifications, even when accompanying hunks match. A `0` exit code therefore does **not** mean the patch can be applied. Use `git apply --check` to check applicability when working with Git patches.
