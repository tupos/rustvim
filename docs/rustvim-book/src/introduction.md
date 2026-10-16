# Introduction

This book explains the RustVim implementation by following its source files
and the operations they perform.

The initial investigation uses commit:

```text
26776968a91b8f80de4f93c93c2b3417baf36e6e
[rust] Unify rendering-system lifecycle
```

At this revision, grid storage and compositor operations are implemented in
Rust. Window-line rendering is still controlled by C's `win_line()`.

All source paths in this book are relative to the repository root.

## Reading and extending the book

Each source chapter records what a file does, the data it contains, and how
other code uses it. Add chapters as files are investigated.

The revision contains 45 Rust files. This is a growing walkthrough; files
without chapters have not yet been covered here.

We skipped detailed walkthroughs of `defines.rs`, `globals.rs`, `mbyte.rs`,
`option.rs`, `pos.rs`, and `generated/`.

Descriptions must be checked against the source when the implementation
changes. Older migration documents may describe a different revision.
An idea raised during investigation is not an adopted design until it has
been reviewed as such.
