# The RustVim Book

Developer documentation built from our investigation of the RustVim source.

From the repository root, build the HTML with:

```sh
mdbook build docs/rustvim-book
```

The entry page is `docs/rustvim-book/book/index.html`.

For a local server that rebuilds after edits:

```sh
mdbook serve docs/rustvim-book
```

Open the address printed by mdBook.

Add new chapters to `src/SUMMARY.md` as the investigation proceeds.
Generated HTML is ignored by Git.
