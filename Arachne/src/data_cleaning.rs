use ego_tree::NodeRef;
use scraper::node::Node::Element;
use scraper::{Html, Node};

const KEEP_ATTRS: [&str; 6] = ["class", "id", "type", "name", "action", "style"];
const VOID_TAGS: [&str; 14] = [
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source",
    "track", "wbr",
];

// Helper enum to simulate recursion state
enum TraversalState<'a> {
    Enter(NodeRef<'a, Node>),
    Leave(&'a str), // Stores tag_name for the closing tag
}

pub fn extract_skeleton_from_doc(document: &Html) -> String {
    let mut buffer = String::with_capacity(8192); // Increased initial capacity
    let mut stack = Vec::with_capacity(64); // The "Heap Stack"

    // Start with the root
    stack.push(TraversalState::Enter(document.tree.root()));

    while let Some(state) = stack.pop() {
        match state {
            TraversalState::Enter(node) => {
                if let Element(element) = node.value() {
                    let tag_name = &*element.name.local;

                    // 1. Build Opening Tag
                    buffer.push('<');
                    buffer.push_str(tag_name);

                    // 2. Attributes
                    for (name, value) in element.attrs() {
                        if KEEP_ATTRS.contains(&name) {
                            buffer.push(' ');
                            buffer.push_str(name);
                            buffer.push_str("=\"");
                            buffer.push_str(value);
                            buffer.push('"');
                        }
                    }

                    // 3. Handle Void Tags (Self-closing)
                    if VOID_TAGS.contains(&tag_name) {
                        buffer.push_str(" />");
                        continue; // No children, no closing tag needed
                    }

                    buffer.push('>');

                    // 4. Schedule Closing Tag (Pushed BEFORE children so it pops AFTER children)
                    stack.push(TraversalState::Leave(tag_name));

                    // 5. Schedule Children
                    // NOTE: specific check to skip SVG internals as per your original logic
                    if tag_name != "svg" {
                        // We must push children in REVERSE order so the first child
                        // is at the top of the stack and processed next.
                        for child in node.children().rev() {
                            stack.push(TraversalState::Enter(child));
                        }
                    }
                } else {
                    // Handle Root Node (Document root is not an Element)
                    if node.parent().is_none() {
                        for child in node.children().rev() {
                            stack.push(TraversalState::Enter(child));
                        }
                    }
                }
            }
            TraversalState::Leave(tag_name) => {
                // 6. Build Closing Tag
                buffer.push_str("</");
                buffer.push_str(tag_name);
                buffer.push('>');
            }
        }
    }

    buffer
}