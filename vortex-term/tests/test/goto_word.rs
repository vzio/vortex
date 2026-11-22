//! `gw` with case: the case of the two typed label letters picks the cursor's side and the WORD
//! around the labeled word (with the default, lowercase alphabet).

use super::*;

/// `ab` selects the word, the cursor at its end.
#[tokio::test(flavor = "multi_thread")]
async fn lowercase_label_selects_the_word_head_at_the_end() -> anyhow::Result<()> {
    test(("#[#|]# foo bar baz\n", "gwab", "# foo #[bar|]# baz\n")).await
}

/// `Ab` selects the word, the cursor at its start.
#[tokio::test(flavor = "multi_thread")]
async fn uppercase_first_letter_puts_the_cursor_at_the_start() -> anyhow::Result<()> {
    test(("#[#|]# foo bar baz\n", "gwAb", "# foo #[|bar]# baz\n")).await
}

/// `aB` on `bar` of `foo.bar` selects the WORD `foo.bar`, the cursor at its end.
#[tokio::test(flavor = "multi_thread")]
async fn uppercase_second_letter_selects_the_word_around() -> anyhow::Result<()> {
    test(("#[#|]# foo.bar baz\n", "gwaB", "# #[foo.bar|]# baz\n")).await
}

/// `AB` selects the same WORD, the cursor at its start.
#[tokio::test(flavor = "multi_thread")]
async fn both_uppercase_selects_the_word_around_head_at_the_start() -> anyhow::Result<()> {
    test(("#[#|]# foo.bar baz\n", "gwAB", "# #[|foo.bar]# baz\n")).await
}

/// A WORD in code runs over punctuation up to the spaces.
#[tokio::test(flavor = "multi_thread")]
async fn the_word_around_punctuation_in_code() -> anyhow::Result<()> {
    // labels: let aa, self ab, items ac, idx ad, len ae
    test((
        "#[ |]# let x = self.items[idx].len();\n",
        "gwaC",
        "  let x = #[self.items[idx].len();|]#\n",
    ))
    .await
}

/// A WORD at the start and the end of a line, in parentheses and in quotes.
#[tokio::test(flavor = "multi_thread")]
async fn the_word_around_edges_parentheses_and_quotes() -> anyhow::Result<()> {
    // start of a line: ab aa? words: ab.cd (aa, ab), ef (ac)
    test(("#[#|]# x\nab.cd ef\n", "gwaB", "# x\n#[ab.cd|]# ef\n")).await?;
    // end of a line: ef aa, gh ab, ij ac
    test((
        "#[#|]# x\nef gh.ij\nzz\n",
        "gwaC",
        "# x\nef #[gh.ij|]#\nzz\n",
    ))
    .await?;
    // parentheses: call aa, foo ab, bar ac
    test((
        "#[#|]# call (foo.bar) end\n",
        "gwaC",
        "# call #[(foo.bar)|]# end\n",
    ))
    .await?;
    // quotes: say aa, hello ab, world ac
    test((
        "#[#|]# say \"hello.world\" end\n",
        "gwaC",
        "# say #[\"hello.world\"|]# end\n",
    ))
    .await
}

/// Select mode, extending forward with each case: the anchor stays at the selection's start.
#[tokio::test(flavor = "multi_thread")]
async fn select_mode_extends_forward_with_each_case() -> anyhow::Result<()> {
    // labels: ddd aa, bbb ab, fff ac, aaa ad, eee ae
    let start = "aaa bbb #[ccc|]# ddd.fff eee\n";
    test((start, "vgwaa", "aaa bbb #[ccc ddd|]#.fff eee\n")).await?;
    test((start, "vgwAa", "aaa bbb #[ccc d|]#dd.fff eee\n")).await?;
    test((start, "vgwaA", "aaa bbb #[ccc ddd.fff|]# eee\n")).await?;
    test((start, "vgwAA", "aaa bbb #[ccc d|]#dd.fff eee\n")).await?;
    // the target is fff, deeper in its WORD
    test((start, "vgwac", "aaa bbb #[ccc ddd.fff|]# eee\n")).await?;
    test((start, "vgwAc", "aaa bbb #[ccc ddd.f|]#ff eee\n")).await?;
    test((start, "vgwaC", "aaa bbb #[ccc ddd.fff|]# eee\n")).await?;
    test((start, "vgwAC", "aaa bbb #[ccc d|]#dd.fff eee\n")).await
}

/// Select mode, extending backward: the anchor moves to the selection's end.
#[tokio::test(flavor = "multi_thread")]
async fn select_mode_extends_backward() -> anyhow::Result<()> {
    // labels: ddd aa, bbb ab, aaa ac
    let start = "aaa x.bbb #[ccc|]# ddd\n";
    test((start, "vgwab", "aaa x.bb#[|b ccc]# ddd\n")).await?;
    test((start, "vgwAb", "aaa x.#[|bbb ccc]# ddd\n")).await?;
    test((start, "vgwaB", "aaa x.bb#[|b ccc]# ddd\n")).await?;
    test((start, "vgwAB", "aaa #[|x.bbb ccc]# ddd\n")).await
}

/// With an alphabet that has uppercase letters, characters are taken as they are: no WORD, no
/// flipped cursor.
#[tokio::test(flavor = "multi_thread")]
async fn an_alphabet_with_uppercase_letters_keeps_the_old_behaviour() -> anyhow::Result<()> {
    let builder = || AppBuilder::new().with_config(test_config_with_alphabet());
    // labels: one aa, two ab, three aA, four aB, five ba
    test_with_config(
        builder(),
        (
            "#[#|]# one.x two three.y four five\n",
            "gwaB",
            "# one.x two three.y #[four|]# five\n",
        ),
    )
    .await?;
    test_with_config(
        builder(),
        (
            "#[#|]# one.x two three.y four five\n",
            "gwaA",
            "# one.x two #[three|]#.y four five\n",
        ),
    )
    .await
}

fn test_config_with_alphabet() -> Config {
    let mut config = test_config();
    config.editor.jump_label_alphabet = vec!['a', 'b', 'A', 'B'];
    config
}

/// A modifier other than Shift cancels `gw`: nothing changes, after either letter.
#[tokio::test(flavor = "multi_thread")]
async fn a_modifier_other_than_shift_cancels() -> anyhow::Result<()> {
    test(("#[#|]# foo bar baz\n", "gw<A-a>", "#[#|]# foo bar baz\n")).await?;
    test(("#[#|]# foo bar baz\n", "gwa<A-b>", "#[#|]# foo bar baz\n")).await?;
    test(("#[#|]# foo bar baz\n", "gw<C-a>", "#[#|]# foo bar baz\n")).await
}

/// Lowercase labels behave as before: the word, cursor at its end, also in the middle of a line.
#[tokio::test(flavor = "multi_thread")]
async fn lowercase_labels_keep_the_upstream_behaviour() -> anyhow::Result<()> {
    // labels: let aa, foo ab, bar ac, baz ad
    let start = "#[ |]# let foo = bar + baz;\n";
    test((start, "gwac", "  let foo = #[bar|]# + baz;\n")).await?;
    test((start, "gwab", "  let #[foo|]# = bar + baz;\n")).await?;
    test((start, "gwaa", "  #[let|]# foo = bar + baz;\n")).await?;
    test((start, "gwad", "  let foo = bar + #[baz|]#;\n")).await
}
