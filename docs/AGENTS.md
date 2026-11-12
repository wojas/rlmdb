# Writing the documentation site

This directory holds the user documentation, published as a website. This
file says how to write for it. It applies to every page listed under
`nav` in `zensical.toml`. The root `AGENTS.md` still applies.

The pages under `getting-started/` set the style. Read one before you
write a new page.

## Who reads it

Developers and operators who want to use rlmdb. They know their language,
their build tools and usually LMDB. They do not know rlmdb's code, and
they should not need to.

They come with a task: run this program on rlmdb, link this library,
store and find these records. A page is good when they finish the task
without reading anything twice.

## What rlmdb is, and in which order

rlmdb is first a replacement for the C LMDB library: for C programs, and
for programs in other languages that use LMDB through its C API. The
Rust API is supported, but it comes second.

Introduce rlmdb that way. Do not open with "a Rust library" or "LMDB in
Rust": readers take that to mean a library for Rust programs. Where a
page lists the parts or the ways to use rlmdb, the C library and the
stubs come first and the Rust library comes last.

## A practical guide, not a specification

The site shows how to do things. The reference documents say exactly
what holds: `COMPATIBILITY.md`, `rlmdb-capi/DETAILS.md`,
`KNOWN_C_BUGS.md` and the header comments. Keep the two apart.

- **Cover the common path.** Describe what most readers do, with
  commands and code that work as shown.
- **Leave out exceptions most readers never meet.** If one matters to
  some readers, give it one sentence and link to the reference. Do not
  list every flag, error code and corner case.
- **Mention a pitfall only if readers will hit it.** "The directory must
  exist" belongs on the page. The behaviour of a NULL comparison
  function does not.
- **Keep getting-started pages to what most programs need.** Plain
  key/value databases, transactions, cursors and range queries.
  Specialised features, such as databases with duplicate keys
  (`MDB_DUPSORT`), get their own guides later.
- **Never contradict the reference.** A guide may say less. It may not
  say something else. Check each statement against the reference, the
  code or a test before you write it.

## Voice

Write the way you would explain it to a colleague at their desk.

- Address the reader as "you". Use the present tense and the active
  voice.
- Use short sentences with one idea each. Prefer common words.
- Use the technical terms readers know: transaction, cursor, rpath,
  snapshot, page size. Do not explain them and do not replace them with
  simpler words. Explain rlmdb's own terms once, where they first appear
  (*stub*, *environment*, *feature*).
- State things plainly. Leave out "simply", "just", "easily", "note
  that", "it is worth mentioning", "as you can see".
- No sales talk, no jokes, no rhetorical questions, no "let's".
- Say what happens, then why, if the reason helps the reader decide.
- Start a page with what the reader gets from it, in two or three
  sentences.

Examples:

- Not: "It should be noted that the environment directory is required
  to exist prior to opening."
  Write: "The directory must exist."
- Not: "rlmdb leverages a stub library mechanism to facilitate seamless
  substitution."
  Write: "A stub is rlmdb under the name `liblmdb`."
- Not: "Simply set the variable and you're good to go!"
  Write: "Set the variable."
- Not: "A new environment that requests no format is created in the
  library's default format (0.9 in `librlmdb` and the 0.9 stub, 1.0 in
  the 1.0 stub), unless overridden."
  Write: "A new environment is 0.9. The 1.0 stub creates 1.0
  environments."

## Page structure

- One page, one task. The title names the task: "Use rlmdb in a C
  program", not "C API".
- Headings are short and say what the section does: "Compile and link",
  "Search with a range query". Number the headings only when the reader
  must follow the steps in order.
- Put the most common case first. Background and options come after the
  reader has something that works.
- Keep paragraphs to a few lines. Use a list for parallel items.
- End with where the details are, as a link.

## Code and commands

- Every command and program on the site must work as shown. Run it
  before you commit, and show the output where it tells the reader that
  it worked.
- Give complete programs for the first example of a page, and short
  fragments after that. Say where a fragment goes.
- Use one small, realistic data set per page and stay with it. The
  getting-started pages store users by name.
- Examples teach good practice. They keep their records in a named
  database, not in the environment's main database, and they check
  every return code.
- Use real paths and names: `/usr/local/lib/rlmdb/lmdb-0.9`, not
  `<stub-dir>`.
- Where platforms differ, use tabs: "Linux and BSD", "macOS", "Windows".
  Keep these labels, so that the reader's choice carries over to every
  page.
- Comments in examples say what the reader needs to know, in a few
  words.

## Diagrams

Use a diagram when it shows something that takes a paragraph to say: who
talks to whom, which path is taken, where a cursor lands. Do not add one
for decoration.

- Draw flows and structures with Mermaid (a `mermaid` code block). It
  follows the light and dark theme and is edited as text.
- Keep a diagram to about eight boxes, with a few words in each. Label
  the arrows where the label carries the meaning.
- For data layouts, cursor positions and timelines, a small text figure
  in a `text` code block is often clearer than a drawing. Where columns
  must line up, use plain ASCII characters: block symbols are wider than
  letters in some fonts.
- The text next to the diagram must make sense without it.

## Tables

Use a table to look something up: a few rows, a few words per cell.

- Three columns at most, about six rows at most.
- No sentences that wrap over several lines in a cell. If the content
  needs explaining, write a list or a section with headings.
- Do not use a table to compare C and rlmdb case by case. That belongs
  in the reference.

## Callouts

Use a callout (`!!! note`, `!!! warning`) for something that would
interrupt the text but that some readers need. At most one or two on a
page. Use `!!! danger` where a mistake loses or damages data, and put it
before the reader can make the mistake. Use a collapsed one (`??? note`) for material few readers need,
such as static linking.

## References to the implementation

Readers use rlmdb, they do not read its source. Leave out module names,
internal function names, test names and line numbers.

Link to a file in the repository only to point the reader to more
detail: a header, a reference document, a script they run. Use the full
URL, `https://github.com/wojas/rlmdb/blob/main/...`, because files
outside `docs/` are not part of the site. For the `mdb_*` functions, link
to the LMDB reference at <http://www.lmdb.tech/doc/>.

## Words

| Use | Not |
|---|---|
| rlmdb (lower case, also at the start of a sentence) | RLMDB, Rlmdb |
| C LMDB | upstream, the reference implementation, liblmdb (for the project) |
| environment | database (for the directory), env |
| stub | shim, wrapper |
| format 0.9, format 1.0 | version 1, data version 3, v10 |
| feature | option, flag (for `split-freelist` and its kind) |
| freelist | free list, free-list |

Keep "abort" and "roll back" apart. A program *aborts* a write
transaction that is still open, which is ordinary and always works. Only
`mdb_env_rollback` *rolls back*, and always say what: "rolling back an
already committed transaction". Readers take a bare "rollback" for an
abort.

## Formatting

- Markdown, wrapped at about 72 columns.
- Only the extensions enabled in `zensical.toml`: tabs, callouts, code
  blocks with titles, Mermaid, tables. Add an extension only when a page
  needs it.
- Code blocks name their language. Use `console` for a command with its
  output, `sh` for commands alone, `text` for output alone.
- Link between pages with relative paths to the `.md` file.

## Building the site

The site is built with [Zensical](https://zensical.org). The release is
pinned in `requirements.txt`. `scripts/build-docs.sh` builds everything
into `site/`, which is not committed:

```sh
scripts/build-docs.sh            # the pages and the Rust API reference; what CI runs
scripts/build-docs.sh --no-api   # the pages only, which is quicker
```

The script builds in strict mode, which fails on broken links between
pages. Run it before you commit.

For a preview with live reload while you write:

```sh
uvx --with-requirements docs/requirements.txt zensical serve
```

Without `uv`: `pip install -r docs/requirements.txt` in a virtual
environment, then `zensical serve`. Run it from the repository root.

The Rust API reference is rustdoc's output for the `rlmdb` crate. The
script puts it under `site/api/`. Link to it with a relative path, as to
a page: `../api/rlmdb/index.html`. In the live preview these links lead
nowhere.

## Publishing

The site is published at <https://rlmdb.wojas.nl/> by
`.github/workflows/docs.yml`. Every push to main that changes the pages
or the `rlmdb` crate builds the site and replaces the published one.
Pull requests are built but not published.

## Adding a page

1. Create the Markdown file under `docs/`, in the directory of its
   section.
2. Add it to `nav` in `zensical.toml`. A page that is not listed there
   does not appear in the navigation.
3. Run every command and example on the page.
4. Build the site with `scripts/build-docs.sh --no-api`.

## Other files in this directory

`DETAILS.md`, `split-freelist.md` and `split-freelist-details.md` are
reference documents that are read in the repository. They are not pages
of the site (`exclude_docs` in `zensical.toml`), and this guide does not
apply to them.
