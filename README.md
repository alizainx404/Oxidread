# oxidread

**A pure Rust, memory-safe rewrite of GNU Readline and ncurses**

Built for **Zainium OS** — Open source for the entire Linux community  
**License:** GPL-3.0-or-later

> "What GNU readline and ncurses achieved in decades of C code with raw pointers and global state,  
> oxidread delivers in safe, modern Rust — with proper Unicode support, typed errors, and zero `unsafe` code."

— Ali Zain, author

## What is oxidread?

`oxidread` is a single Rust crate that provides a modern, safe alternative to two foundational terminal libraries:

- **Readline Engine**: Full-featured interactive line editing (history, completion, Emacs/Vi bindings, kill ring, incremental search, undo)
- **ncurses Replacement**: Pure Rust TUI (Terminal User Interface) with windows, colors, attributes, and screen management — **without any C dependencies**

Both components are designed to work seamlessly together. No global state. No unsafe code. Full Unicode support from the ground up.

Perfect for building shells, configuration tools, menu-driven interfaces, and minimal Linux distributions like **Zainium OS**.

## Why oxidread?

GNU Readline (1987) and ncurses are powerful but outdated in several ways:

- Heavy global mutable state
- Raw pointer arithmetic and memory safety issues
- Poor default Unicode handling
- Difficult to embed or use multiple instances safely
- Hard dependency on C libraries (`libncurses`, `libtinfo`)

`oxidread` solves these problems using Rust’s strengths:

- Ownership and type system
- Grapheme-cluster aware text handling
- Typed errors instead of magic return codes
- Zero C dependencies (musl-friendly)

## Current Status

- **58 tests passing**
- **0 `unsafe` blocks**
- **0 C library dependencies**
- Ready for musl static builds

**Phase 1 (Core Readline)** — In Progress  
**Phase 2 (Advanced Readline)** — Planned  
**Phase 3 (ncurses Core)** — Planned  
**Phase 4 (Integration + C ABI)** — Planned

## Architecture

```text
oxidread/
└── src/
    ├── lib.rs
    ├── readline/
    │   ├── mod.rs
    │   ├── error.rs
    │   ├── line_buffer.rs      # Unicode-aware text buffer
    │   ├── history.rs
    │   ├── prompt.rs
    │   ├── keymaps.rs
    │   ├── completion.rs
    │   └── editor.rs           # Main readline() API
    ├── ncurses/
    │   ├── mod.rs
    │   ├── screen.rs
    │   ├── window.rs
    │   └── attributes.rs
    └── integration/
        └── mod.rs              # Readline inside ncurses windows
