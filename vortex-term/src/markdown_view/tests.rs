//! Tests of the renderer: snapshots of the page for each kind of Markdown, and checks of the
//! styles the theme gives its pieces.
//!
//! A snapshot is the page's lines without their trailing spaces, in
//! `snapshots/<name>.txt`. Run with `UPDATE_SNAPSHOTS=1` to write them again (a missing one is
//! written too, and the test fails), and look at the result before keeping it.

use std::path::Path;

use vortex_view::theme::{Loader, Modifier};
use vortex_view::Theme;

use super::page::{Page, Widths};

fn loader() -> vortex_core::syntax::Loader {
    vortex_core::config::default_lang_loader()
}

/// The page of `source` at `width` as text, for the tests.
fn render(source: &str, width: usize) -> Vec<String> {
    render_at(source, Widths::uniform(width))
}

fn render_at(source: &str, widths: Widths) -> Vec<String> {
    let theme = Theme::default();
    Page::new(source, widths, &theme, &loader())
        .lines
        .iter()
        .map(|line| line.text().trim_end().to_string())
        .collect()
}

/// The page with the theme `name` (a built-in one).
fn themed(source: &str, width: usize, name: &str) -> Page {
    let theme = Loader::new(&[]).load(name).unwrap();
    Page::new(source, Widths::uniform(width), &theme, &loader())
}

fn theme_named(name: &str) -> Theme {
    Loader::new(&[]).load(name).unwrap()
}

/// Compares the page of `source` at `width` with `snapshots/<name>.txt`, or writes it there
/// when it is missing or `UPDATE_SNAPSHOTS` is set (and then fails when it was missing).
fn snapshot(name: &str, source: &str, width: usize) {
    snapshot_at(name, source, Widths::uniform(width));
}

fn snapshot_at(name: &str, source: &str, widths: Widths) {
    let mut drawn = render_at(source, widths).join("\n");
    drawn.push('\n');
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src/markdown_view/snapshots")
        .join(format!("{name}.txt"));
    let update = std::env::var_os("UPDATE_SNAPSHOTS").is_some();
    if update || !path.exists() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &drawn).unwrap();
        assert!(
            update,
            "{name}: no snapshot at {}, written: look at it, then run again",
            path.display()
        );
        return;
    }
    let expected = std::fs::read_to_string(&path).unwrap();
    assert!(
        expected == drawn,
        "{name} differs from {}, run with UPDATE_SNAPSHOTS=1 to update it:\n{drawn}",
        path.display()
    );
}

const HEADINGS: &str = "\
# Vortex
## Installing
### From source
#### Build options
##### Feature flags
###### Notes on `--no-default-features`
";

#[test]
fn headings_one_to_six() {
    snapshot("headings", HEADINGS, 60);
}

#[test]
fn a_long_heading_wraps() {
    snapshot(
        "heading-wrapped",
        "## A heading that is much longer than the page is wide, so it wraps\n",
        30,
    );
}

#[test]
fn emphasis_strong_strikethrough_and_code_lose_their_markers() {
    snapshot(
        "inline",
        "\
Some *emphasis*, some **strong text**, a ~~removed~~ word, `inline_code()` and
***both at once***. Underscores work too: _this_ and __that__.
",
        60,
    );
}

#[test]
fn nested_lists_have_a_bullet_per_depth() {
    snapshot(
        "list-bullets",
        "\
- Editors
  - Modal
    - Kakoune
    - Vim
  - Modeless
- Terminals
  - Fast ones
",
        60,
    );
}

#[test]
fn ordered_lists_align_their_numbers_on_the_right() {
    snapshot(
        "list-ordered",
        "\
8. Eighth step
9. Ninth step
10. Tenth step, which is long enough to wrap onto a second line of the page
11. Eleventh step

A second list starts over:

1. Install
2. Configure
   1. Edit the config
   2. Reload it
",
        50,
    );
}

#[test]
fn task_lists_have_boxes() {
    snapshot(
        "list-tasks",
        "\
- [x] Write the parser
- [ ] Lay the page out
- [x] Wrap the text, and keep the box on the first line when an item is long enough to wrap
  - [ ] a nested task
",
        50,
    );
}

#[test]
fn a_loose_list_has_blank_lines_between_its_items() {
    snapshot(
        "list-loose",
        "\
- First, a paragraph of its own.

- Second, with more to say than fits in one line of the page.

- Third.
",
        40,
    );
}

#[test]
fn quotes_have_a_bar() {
    snapshot(
        "quote",
        "\
> The best way to predict the future is to invent it.
>
> > A nested quote, with a second bar.
>
> - and a list
",
        40,
    );
}

#[test]
fn alerts_have_their_name() {
    snapshot(
        "alert",
        "\
> [!NOTE]
> Useful information that users should know.

> [!WARNING]
> Urgent info that needs immediate attention to avoid problems.
",
        50,
    );
}

#[test]
fn a_rule_spans_the_page() {
    snapshot("rule", "Above\n\n---\n\nBelow\n", 30);
}

const CODE: &str = "\
Run it:

```rust
fn main() {
    let message = \"a very long line of code that does not fit in the page and is never wrapped\";
    println!(\"{message}\");
}
```
";

#[test]
fn a_code_block_is_not_wrapped() {
    snapshot("code", CODE, 40);
    let page = render(CODE, 40);
    assert!(
        page.iter().any(|line| line.contains(
            "a very long line of code that does not fit in the page and is never wrapped"
        )),
        "{page:#?}"
    );
}

const TABLE: &str = "\
| Key | Command | Meaning |
|:----|:-------:|--------:|
| `V` | markdown_view | Toggle the rendered page |
| `]` | next_heading | Next heading |
| `t` | outline | The outline of the page, as a picker you can filter |
";

#[test]
fn a_table_narrower_than_the_page_keeps_its_cells_on_one_line() {
    snapshot("table-wide", TABLE, 100);
}

#[test]
fn a_table_wider_than_the_page_wraps_its_cells() {
    snapshot("table-narrow", TABLE, 44);
}

#[test]
fn a_table_has_rules_between_the_rows_that_wrap() {
    let page = render(TABLE, 44);
    let rules = page.iter().filter(|line| line.contains('─')).count();
    assert!(rules >= 2, "{page:#?}");
    snapshot("table-narrow-30", TABLE, 30);
}

#[test]
fn links_show_their_address_after_the_text() {
    snapshot(
        "links",
        "\
Read [the guide](https://example.com/guide) first, then see [below](#details)
or <https://example.com/auto> and [a local page](docs/setup.md).

## Details
",
        70,
    );
}

#[test]
fn footnotes_come_at_the_end() {
    snapshot(
        "footnotes",
        "\
Vortex[^1] keeps the keymap of its parent[^note].

More text after the references.

[^1]: A fork, with its own features.
[^note]: Upstream is credited in the README.
",
        50,
    );
}

#[test]
fn an_image_and_html_are_shown_as_they_are() {
    snapshot(
        "image-html",
        "\
![the logo](logo.svg)

Text with <kbd>Ctrl</kbd> inline.

<details>
<summary>More</summary>
Hidden text
</details>
",
        50,
    );
}

const PARAGRAPH: &str = "\
Vortex is a modal editor for the terminal. It keeps what makes its parent fast, selections before actions, and adds a rendered view of Markdown, a git log, and a trainer.

A second paragraph, shorter.
";

#[test]
fn a_paragraph_wraps_at_forty() {
    snapshot("width-40", PARAGRAPH, 40);
}

#[test]
fn a_paragraph_wraps_at_eighty() {
    snapshot("width-80", PARAGRAPH, 80);
}

// the styles

fn pieces_of<'a>(page: &'a Page, text: &str) -> Vec<&'a super::wrap::Piece> {
    page.lines
        .iter()
        .flat_map(|line| &line.pieces)
        .filter(|piece| piece.text.contains(text))
        .collect()
}

#[test]
fn a_level_one_heading_has_the_band_of_the_theme() {
    for name in ["vortex", "matrix"] {
        let theme = theme_named(name);
        let heading = theme.get("markup.heading.1");
        assert!(heading.bg.is_some(), "{name} has a band on level 1");
        let page = themed("# Title\n\nBody\n", 40, name);
        let line = &page.lines[0];
        let (column, _, fill) = line.fill.expect("the band");
        assert_eq!(column, 0);
        assert_eq!(fill.bg, heading.bg, "{name}");
        for piece in &line.pieces {
            assert_eq!(piece.style.fg, heading.fg, "{name}: {piece:?}");
        }
        // the body has no band
        assert!(page.lines[2].fill.is_none());
    }
}

#[test]
fn headings_four_to_six_have_no_band() {
    let page = themed("#### Four\n\n##### Five\n\n###### Six\n", 40, "vortex");
    assert!(page.lines.iter().all(|line| line.fill.is_none()));
    assert_eq!(page.headings.len(), 3);
}

#[test]
fn strikethrough_is_crossed_out() {
    for name in ["vortex", "matrix"] {
        let page = themed("a ~~old~~ b\n", 40, name);
        let pieces = pieces_of(&page, "old");
        assert_eq!(pieces.len(), 1, "{name}");
        assert!(
            pieces[0].style.add_modifier.contains(Modifier::CROSSED_OUT),
            "{name}"
        );
        // the text around it is not
        let plain = pieces_of(&page, "a ");
        assert!(!plain[0].style.add_modifier.contains(Modifier::CROSSED_OUT));
    }
}

#[test]
fn emphasis_is_italic_and_strong_is_bold() {
    let page = themed("*slanted* and **heavy**\n", 40, "vortex");
    let slanted = pieces_of(&page, "slanted");
    assert!(slanted[0].style.add_modifier.contains(Modifier::ITALIC));
    assert!(!slanted[0].style.add_modifier.contains(Modifier::BOLD));
    let heavy = pieces_of(&page, "heavy");
    assert!(heavy[0].style.add_modifier.contains(Modifier::BOLD));
    assert!(!heavy[0].style.add_modifier.contains(Modifier::ITALIC));
}

#[test]
fn an_image_and_html_are_dim() {
    let theme = theme_named("vortex");
    let dim = theme.get("ui.markdown.dim");
    assert!(dim.fg.is_some());
    let page = themed(
        "![alt text](pic.png)\n\nA <kbd>key</kbd>.\n\n<div>\nraw\n</div>\n",
        40,
        "vortex",
    );
    for text in ["[image: alt text]", "<kbd>", "<div>", "raw"] {
        let pieces = pieces_of(&page, text);
        assert!(!pieces.is_empty(), "{text}");
        for piece in pieces {
            assert_eq!(piece.style.fg, dim.fg, "{text}");
        }
    }
}

#[test]
fn code_block_lines_have_the_background_of_the_theme() {
    let theme = theme_named("vortex");
    let block = theme.get("markup.raw.block");
    assert!(block.bg.is_some());
    let page = themed("Before\n\n```\none\ntwo\n```\n\nAfter\n", 40, "vortex");
    let code: Vec<_> = page
        .lines
        .iter()
        .filter(|line| line.code.is_some())
        .collect();
    assert_eq!(code.len(), 2);
    for line in code {
        let (_, _, fill) = line.fill.expect("a fill");
        assert_eq!(fill.bg, block.bg);
        assert_eq!(line.code, Some(0));
    }
    assert!(page.lines[0].code.is_none() && page.lines[0].fill.is_none());
    let last = page.lines.last().unwrap();
    assert!(last.code.is_none() && last.fill.is_none());
}

#[test]
fn a_wide_code_block_scrolls() {
    let long = "x".repeat(70);
    let source = format!("```\nshort\n{long}\n```\n\n```\nfits\n```\n");
    let page = themed(&source, 40, "vortex");
    assert_eq!(page.code_blocks.len(), 2);
    let wide = &page.code_blocks[0];
    assert!(wide.scrolls());
    assert_eq!(wide.widest, 70);
    // the width less the margin cells for the markers: 40 - 1 (the padding) - 2 ... of the
    // code's width
    assert!(wide.visible < 40 && wide.visible >= 36, "{}", wide.visible);
    assert_eq!(wide.max_offset(), 70 - wide.visible);
    assert_eq!(wide.lines, 0..2);
    assert_eq!(wide.column, 1);
    let narrow = &page.code_blocks[1];
    assert!(!narrow.scrolls());
    assert_eq!(narrow.widest, 4);
    assert_eq!(narrow.max_offset(), 0);
    // the lines tell their block
    assert_eq!(page.lines[0].code, Some(0));
    assert_eq!(page.lines[3].code, Some(1));
}

#[test]
fn headings_have_the_anchors_of_github() {
    let page = themed(
        "# Hello, World!\n\n## The `V` key (Markdown)\n\n## Hello, World!\n\n## Ünïcode & more_stuff\n",
        60,
        "vortex",
    );
    let anchors: Vec<_> = page.headings.iter().map(|h| h.anchor.as_str()).collect();
    assert_eq!(anchors[0], "hello-world");
    assert_eq!(anchors[1], "the-v-key-markdown");
    assert_eq!(anchors[3], "ünïcode--more_stuff");
    // a second heading of the same text gets a suffix, as on GitHub, or at least another one
    assert_ne!(anchors[0], anchors[2], "{anchors:?}");
    assert_eq!(page.anchor("hello-world"), Some(0));
    assert_eq!(page.anchor("Hello-World"), Some(0));
    assert_eq!(
        page.anchor("the-v-key-markdown"),
        Some(page.headings[1].line)
    );
    // as written in a link, percent-encoded
    assert_eq!(
        page.anchor("%C3%9Cn%C3%AFcode-%26-more_stuff"),
        Some(page.headings[3].line)
    );
    assert_eq!(page.anchor("nothing"), None);
    assert_eq!(page.headings[1].text, "The V key (Markdown)");
    assert_eq!(page.headings[1].level, 2);
}

#[test]
fn the_source_lines_map_back_to_the_page() {
    let source = "# Title\n\nFirst paragraph\nwith two lines.\n\n```\ncode\n```\n\n- item\n";
    let page = themed(source, 60, "vortex");
    assert_eq!(page.line_of_source(0), 0);
    // line 3 (from 0) is the second line of the paragraph: a page line comes from it, or the
    // page line before
    let paragraph = page.line_of_source(2);
    assert_eq!(page.lines[paragraph].source, 2);
    assert!(page.line_of_source(3) >= paragraph);
    // the code's line
    let code = page.line_of_source(6);
    assert_eq!(page.lines[code].text().trim(), "code");
    assert!(page.lines[code].code.is_some());
    // the item
    // (the blank line before a block has the block's source line: the first line of it)
    let item = page.line_of_source(9);
    assert_eq!(page.lines[item].source, 9);
    assert!(page.lines[item..]
        .iter()
        .any(|line| line.text().contains("item")));
    // a source line past the end is the last line of the body
    assert_eq!(page.line_of_source(100), page.body_end - 1);
    // and a line with nothing on the page (the blank one) is the line before it
    assert!(page.line_of_source(1) <= page.line_of_source(2));
    for (i, line) in page.lines.iter().enumerate() {
        if line.source > 0 && !line.text().trim().is_empty() {
            assert_eq!(
                page.lines[page.line_of_source(line.source)].source,
                line.source,
                "page line {i}"
            );
        }
    }
}

#[test]
fn links_are_listed_by_line() {
    let page = themed(
        "See [one](https://a.example) and [two](#details).\n\n## Details\n",
        70,
        "vortex",
    );
    let links = page.lines[0].links();
    assert_eq!(links.len(), 2);
    assert_eq!(page.internal_target(&page.links[links[1].2]), Some(2));
    assert_eq!(page.internal_target(&page.links[links[0].2]), None);
}

// the reading width: prose at its own width, tables and code blocks at the window's

fn width_of(lines: &[String]) -> usize {
    use vortex_core::unicode::width::UnicodeWidthStr;
    lines.iter().map(|line| line.width()).max().unwrap_or(0)
}

fn pages(source: &str, widths: Widths) -> Page {
    Page::new(source, widths, &Theme::default(), &loader())
}

const PROSE_AND_TABLE: &str = "\
Vortex renders a Markdown file as a page, and keeps its prose at a comfortable width.

| Command              | What it does                               |
|----------------------|--------------------------------------------|
| markdown_view_toggle | Toggle the rendered page of a Markdown doc |
| goto_next_heading    | Put the next heading at the top of the page |

Back to prose, which wraps at the narrow width again.
";

#[test]
fn a_table_breaks_out_of_the_prose_width() {
    // as wide as it is
    let natural = render(PROSE_AND_TABLE, 200);
    let table: Vec<_> = natural
        .iter()
        .filter(|line| line.starts_with(['┌', '│', '├', '└']))
        .collect();
    assert_eq!(table.len(), 6, "{natural:#?}");
    let table_width = width_of(
        &table
            .iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>(),
    );
    assert_eq!(table_width, 70, "{natural:#?}");

    let widths = Widths {
        prose: 40,
        wide: 100,
    };
    let page = pages(PROSE_AND_TABLE, widths);
    let drawn = render_at(PROSE_AND_TABLE, widths);
    // the prose wraps at 40, the table is whole
    for line in &drawn {
        if !line.starts_with(['┌', '│', '├', '└']) {
            assert!(line.chars().count() <= 40, "{line:?}");
        }
    }
    let rows: Vec<_> = drawn
        .iter()
        .filter(|line| line.starts_with(['┌', '│', '├', '└']))
        .collect();
    assert_eq!(rows, table, "the table is the same as with all the room");
    // 70 columns fit in the right room (100 - 30): no shift
    assert_eq!(widths.left_room(), 30);
    assert!(page.lines.iter().all(|line| line.shift == 0));
    snapshot_at("widths-40-100", PROSE_AND_TABLE, widths);
}

#[test]
fn a_table_wider_than_the_right_room_is_shifted_left() {
    let widths = Widths {
        prose: 40,
        wide: 100,
    };
    // 70 columns, but only 60 on the right of the prose's left edge
    let narrow = Widths {
        prose: 40,
        wide: 80,
    };
    assert_eq!(narrow.left_room(), 20);
    let page = pages(PROSE_AND_TABLE, narrow);
    let table: Vec<_> = page
        .lines
        .iter()
        .filter(|line| line.text().starts_with(['┌', '│', '├', '└']))
        .collect();
    assert!(!table.is_empty());
    for line in &table {
        // extent 70, right room 60: 10 to the left, which the left room has
        assert_eq!(line.shift, 10, "{:?}", line.text());
        assert_eq!(line.extent(), 70);
    }
    // the prose is not shifted
    assert!(page
        .lines
        .iter()
        .filter(|line| !line.text().starts_with(['┌', '│', '├', '└']))
        .all(|line| line.shift == 0));
    let _ = widths;
}

#[test]
fn with_one_width_the_table_wraps_its_cells() {
    let natural = render(PROSE_AND_TABLE, 200);
    let wrapped = render(PROSE_AND_TABLE, 40);
    assert!(wrapped.len() > natural.len());
    for line in &wrapped {
        assert!(line.chars().count() <= 40, "{line:?}");
    }
    assert!(wrapped.iter().any(|line| line.starts_with('├')));
    assert!(pages(PROSE_AND_TABLE, Widths::uniform(40))
        .lines
        .iter()
        .all(|line| line.shift == 0));
    snapshot("widths-uniform-40", PROSE_AND_TABLE, 40);
}

/// A shell command exactly 80 cells wide.
fn command_80() -> String {
    let command = format!("tar czf backup.tar.gz {}", "projects/vortex/".repeat(3));
    let command = format!("{command}{}", "x".repeat(80 - command.len()));
    assert_eq!(command.len(), 80);
    command
}

#[test]
fn a_code_block_takes_the_room_of_the_window() {
    let source = format!(
        "Run this from the project folder:\n\n```sh\n{}\n```\n",
        command_80()
    );
    let widths = Widths {
        prose: 40,
        wide: 100,
    };
    let page = pages(&source, widths);
    let block = &page.code_blocks[0];
    assert_eq!(block.widest, 80);
    assert!(!block.scrolls());
    assert_eq!(block.visible, 80);
    assert_eq!(block.max_offset(), 0);
    let line = page.lines.iter().find(|line| line.code.is_some()).unwrap();
    // its background: the padding, the code, the padding (2 more than the code)
    let (start, end, _) = line.fill.unwrap();
    assert_eq!((start, end), (0, 82));
    // 82 columns, 70 to the right of the prose's left edge: 12 to the left
    assert_eq!(line.shift, 12);
    // the paragraph wraps at 40
    assert!(page.lines[0].text().chars().count() <= 40);
    assert!(page.lines[1].text().chars().count() <= 40);

    // with one width, it scrolls
    let page = pages(&source, Widths::uniform(40));
    let block = &page.code_blocks[0];
    assert!(block.scrolls());
    assert_eq!(block.visible, 38);
    assert_eq!(block.max_offset(), 42);
    let line = page.lines.iter().find(|line| line.code.is_some()).unwrap();
    assert_eq!(line.shift, 0);
    assert_eq!(line.fill.map(|(_, end, _)| end), Some(40));
}

#[test]
fn a_code_block_wider_than_the_window_scrolls() {
    let source = format!("```sh\n{}\n```\n", command_80());
    // 60 columns in all
    let page = pages(
        &source,
        Widths {
            prose: 40,
            wide: 60,
        },
    );
    let block = &page.code_blocks[0];
    assert!(block.scrolls());
    assert_eq!(block.visible, 58);
    let line = &page.lines[0];
    assert_eq!(line.fill.map(|(_, end, _)| end), Some(60));
    assert_eq!(line.shift, 10);
}

// what the page adds to the text is ASCII, or the box-drawing and block characters

const EVERYTHING: &str = "\
# Heading one

## Heading two with `code`

### Three

#### Four

##### Five

###### Six

Plain text with *emphasis*, **strong**, ~~strike~~, `code`, a [link](https://example.com/a),
an [anchor](#heading-one), an <https://example.com/auto> autolink, a footnote[^a],
and an image: ![the logo](logo.svg)

A hard break here\
and after it.

- bullet one
  - nested bullet
    - third level
      - fourth level
- bullet two

1. first
2. second
   1. nested number

- [x] done
- [ ] todo

> A quote
>
> > nested

> [!NOTE]
> A note.

> [!TIP]
> A tip.

> [!IMPORTANT]
> Important.

> [!WARNING]
> Careful.

> [!CAUTION]
> Dangerous.

---

```rust
fn main() { println!(\"hello\"); }
```

    indented code

| Left | Center | Right |
|:-----|:------:|------:|
| a    | b      | c     |
| a long cell that wraps when the window is narrow | b | c |

<div>
raw html
</div>

Text with <kbd>inline</kbd> html.

[^a]: The footnote text.
";

fn is_added_char(c: char) -> bool {
    c.is_ascii() || ('\u{2500}'..='\u{259F}').contains(&c)
}

#[test]
fn the_page_adds_only_ascii_and_box_drawing_characters() {
    assert!(EVERYTHING.is_ascii());
    for widths in [
        Widths::uniform(30),
        Widths::uniform(80),
        Widths {
            prose: 40,
            wide: 100,
        },
        Widths {
            prose: 100,
            wide: 160,
        },
    ] {
        let page = render_at(EVERYTHING, widths);
        assert!(page.len() > 40);
        for line in &page {
            for c in line.chars() {
                assert!(
                    is_added_char(c),
                    "{c:?} (U+{:04X}) in {line:?} at {widths:?}",
                    c as u32
                );
            }
        }
        // (and the constructs are there)
        let text = page.join("\n");
        for expected in [
            "- bullet one",
            "* nested bullet",
            "+ third level",
            "[x] done",
            "[ ] todo",
            "▎",
            "─",
            "┌",
        ] {
            assert!(
                text.contains(expected),
                "{expected:?} at {widths:?}:\n{text}"
            );
        }
    }
}

#[test]
fn the_documents_own_characters_pass_through() {
    let source = "\
# Party time 🎉

Done ✅ with :sob: and :smile: shortcodes, café, 日本語 and `:sob:` in code.

- ❤️ a list item with :tada:

| Mood | Code |
|------|------|
| 😭   | :sob: |
";
    let page = render(source, 80);
    let text = page.join("\n");
    for expected in [
        "Party time 🎉",
        "Done ✅ with :sob: and :smile: shortcodes, café, 日本語 and :sob: in code.",
        "- ❤️ a list item with :tada:",
        "😭",
        ":sob:",
    ] {
        assert!(text.contains(expected), "{expected:?} in:\n{text}");
    }
    // not turned into anything else
    assert!(!text.contains("😢"));
    assert!(text.matches(":sob:").count() >= 3);
}
