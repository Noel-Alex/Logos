use ego_tree::NodeRef;
use scraper::node::Node::Element;
use scraper::{Html, Node};

const KEEP_ATTRS: [&str; 6] = ["class", "id", "type", "name", "action", "style"];
const VOID_TAGS: [&str; 14] = [
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source",
    "track", "wbr",
];

pub fn extract_skeleton_from_doc(document: &Html) -> String {
    let mut skeleton = String::with_capacity(4096);
    walk_and_build(document.tree.root(), &mut skeleton);
    skeleton
}

fn walk_and_build(node: NodeRef<Node>, buffer: &mut String) {
    if let Element(element) = node.value() {
        let tag_name = &*element.name.local;

        buffer.push('<');
        buffer.push_str(tag_name);

        // --- FIXED SECTION START ---
        for (name, value) in element.attrs() {
            // 'name' and 'value' are already &str here
            if KEEP_ATTRS.contains(&name) {
                buffer.push(' ');
                buffer.push_str(name);
                buffer.push_str("=\"");
                buffer.push_str(value);
                buffer.push('"');
            }
        }
        // --- FIXED SECTION END ---

        if VOID_TAGS.contains(&tag_name) {
            buffer.push_str(" />");
            return;
        }

        buffer.push('>');

        if tag_name != "svg" {
            for child in node.children() {
                walk_and_build(child, buffer);
            }
        }

        buffer.push_str("</");
        buffer.push_str(tag_name);
        buffer.push('>');
    } else {
        if node.parent().is_none() {
            for child in node.children() {
                walk_and_build(child, buffer);
            }
        }
    }
}
