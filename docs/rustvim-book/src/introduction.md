# Introduction

RustVim is a gradual rewrite of Neovim in Rust. This book explains its
implementation by following the source code: what each component does,
what data it owns, and how other components use it.

The editor contains both Rust and C code. The chapters describe their
interactions, including how C calls Rust components and which editor
services those components use.

## Reading the book

Start with [Cells, rows, and grids](rendering/basics.md) for the rendering
terminology, then read the window-line chapter to see how buffer text
becomes screen rows.

The Rust source walkthrough explains individual modules, beginning with
the [library root](rust/lib.md). Chapters for files within a module are
grouped beneath that module in the table of contents.

All source paths are relative to the repository root. The walkthrough
covers the files listed in the table of contents and grows as more
components are documented.
