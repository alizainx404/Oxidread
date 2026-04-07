# oxidread

**A pure Rust, memory-safe rewrite of GNU Readline + ncurses**

Built for **Zainium OS** · Open source for the entire Linux ecosystem  
**License:** GPL-3.0-or-later

> "What GNU readline does in ~60,000 lines of C with raw pointers and global state,  
> oxidread does in safe Rust — with Unicode-first design, typed errors, and zero `unsafe` code."

— Ali Zain, author

## What is oxidread?

`oxidread` is a modern, single Rust crate that brings two foundational terminal technologies into safe Rust:

1. **Modern Readline Engine** — Full-featured interactive line editing with:
   - History management & persistence
   - Emacs & Vi key bindings
   - Tab completion
   - Kill ring / yank
   - Incremental search (Ctrl+R)
   - Undo support
   - Full Unicode (grapheme cluster) support from day one

2. **Pure Rust ncurses Alternative** — TUI window management, colors, attributes, and terminal control **without depending on libncurses, libtinfo, or any C library**.

Both components are designed to work together seamlessly in one crate — no global state, no unsafe code, no C dependencies.

## Why Rewrite in Rust?

GNU Readline (1987) and ncurses are legendary, but they suffer from:

- Heavy reliance on global mutable state
- Raw pointer arithmetic and manual memory management
- Poor Unicode handling (multibyte characters are error-prone)
- Difficult to use multiple instances in one process
- Memory safety risks

`oxidread` fixes all of this at the **type level** using Rust’s ownership model, grapheme-aware buffers, and typed errors.

## Current Status

- **58 tests passing**
- **0 `unsafe` blocks**
- **0 C library dependencies**
- **Musl-ready** (perfect for static binaries)

| Module              | Status     | Coverage                  |
|---------------------|------------|---------------------------|
| `error`             | ✅ Done    | Full                      |
| `line_buffer`       | ✅ Done    | 35 tests                  |
| `history`           | ✅ Done    | 14 tests                  |
| `prompt`            | 🔨 In Progress | Phase 1               |
| `keymaps`           | 🔨 In Progress | Phase 1               |
| `completion`        | 🔨 In Progress | Phase 1               |
| `editor`            | 🔨 In Progress | Phase 1               |
| `ncurses` core      | 📋 Planned | Phase 3                   |
| C ABI Layer         | 📋 Planned | Phase 4                   |

## Architecture

```text
oxidread/
└── src/
    ├── lib.rs
    ├── readline/
    │   ├── mod.rs
    │   ├── error.rs
    │   ├── line_buffer.rs      # Unicode grapheme buffer
    │   ├── history.rs
    │   ├── prompt.rs
    │   ├── keymaps.rs
    │   ├── completion.rs
    │   └── editor.rs           # Main readline() API
    ├── ncurses/
    │   ├── mod.rs
    │   ├── screen.rs
    │   └── window.rs
    └── integration/
        └── mod.rs              # Readline inside ncurses windows
