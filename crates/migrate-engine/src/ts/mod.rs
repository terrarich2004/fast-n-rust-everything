//! The TypeScript/JavaScript language plugin: discovery, parsing and the npm crate table.

pub mod deps;
pub mod discover;
pub mod exports;
pub mod jsdoc;
pub mod scan;
pub mod shape;
pub mod tsconfig;

/// The code of a file as TS: the file itself, or only the `<script>` blocks of a
/// `.vue`/`.svelte`/`.astro` component. Text outside scripts becomes blank lines so
/// reported line numbers stay right.
pub fn script_of(path: &str, text: &str) -> String {
    let is_sfc = [".vue", ".svelte", ".astro"]
        .iter()
        .any(|ext| path.ends_with(ext));
    if !is_sfc {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find("<script") {
        out.extend(rest[..open].chars().filter(|&c| c == '\n'));
        let after_open = &rest[open..];
        let Some(tag_end) = after_open.find('>') else {
            break;
        };
        out.extend(after_open[..=tag_end].chars().filter(|&c| c == '\n'));
        let body = &after_open[tag_end + 1..];
        let close = body.find("</script>").unwrap_or(body.len());
        out.push_str(&body[..close]);
        // Two blocks on one line (`<script>a</script><script>b</script>`) must not glue their
        // bodies together, and a newline here would push every later block down a line: a space
        // keeps them apart on the line they are already on.
        if !body[..close].ends_with('\n') {
            out.push(' ');
        }
        rest = &body[close..];
        rest = rest.strip_prefix("</script>").unwrap_or(rest);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::script_of;

    #[test]
    fn keeps_only_scripts_with_line_numbers() {
        let sfc = "<template>\n<div/>\n</template>\n<script setup lang=\"ts\">\nimport x from './x'\n</script>\n";
        let script = script_of("App.vue", sfc);
        assert_eq!(script.lines().nth(4), Some("import x from './x'"));
    }

    #[test]
    fn a_second_block_stays_on_its_own_lines() {
        let sfc = "<script context=\"module\">\nexport const a = 1\n</script>\n<script>\nlet b = 2\n</script>\n";
        let script = script_of("App.svelte", sfc);
        assert_eq!(script.lines().nth(1), Some("export const a = 1"));
        assert_eq!(script.lines().nth(4), Some("let b = 2"), "{script:?}");
    }

    #[test]
    fn two_blocks_on_one_line_do_not_glue_their_bodies() {
        let script = script_of("App.svelte", "<script>a</script><script>b</script>");
        assert!(!script.contains("ab"), "{script:?}");
        assert_eq!(script.lines().count(), 1, "{script:?}");
    }
}
