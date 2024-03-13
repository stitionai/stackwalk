use crate::block::{Block, BlockType};
use crate::config::{Config, Matchers};
use jwalk::rayon::str::ParallelString;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;
use tree_sitter::{Language, Node, Parser};

use crate::indexer::generate_node_key;

extern "C" {
    fn tree_sitter_rust() -> Language;
    fn tree_sitter_python() -> Language;
    // Add more language bindings here
}

pub fn parse_file(file_path: &Path, module_name: &str, config: &Config) -> Vec<Block> {
    let code = fs::read_to_string(file_path).unwrap();
    let language = tree_sitter_language(file_path);
    let mut parser = Parser::new();
    parser.set_language(language).unwrap();
    let tree = parser.parse(&code, None).unwrap();

    let mut blocks = Vec::new();
    let mut non_function_blocks = Vec::new();
    let mut imports = HashMap::new();
    let mut cursor = tree.root_node().walk();

    traverse_tree(
        &code,
        &mut cursor,
        &mut blocks,
        &mut non_function_blocks,
        language,
        None,
        module_name,
        &mut imports,
        &config,
    );

    if !non_function_blocks.is_empty() {
        let non_function_block_content = non_function_blocks.join("\n");
        blocks.push(Block::new(
            String::from("non_function_block"),
            BlockType::NonFunction,
            non_function_block_content,
            None,
            None,
        ));
    }

    blocks
}

fn tree_sitter_language(file_path: &Path) -> Language {
    let extension = file_path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("");
    match extension {
        "rs" => unsafe { tree_sitter_rust() },
        "py" => unsafe { tree_sitter_python() },
        // Add more mappings for other supported languages
        _ => panic!("Unsupported language"),
    }
}

fn traverse_tree(
    code: &str,
    cursor: &mut tree_sitter::TreeCursor,
    blocks: &mut Vec<Block>,
    non_function_blocks: &mut Vec<String>,
    language: Language,
    class_name: Option<String>,
    module_name: &str,
    imports: &mut HashMap<String, String>,
    config: &Config,
) {
    let node = cursor.node();
    let kind = node.kind();

    if is_import_statement(kind, language) {
        if let Some((module, alias)) = parse_import_statement(code, node, language, config) {
            println!("Module: {}, Alias: {}", module, alias);
            imports.insert(alias, module);
        }
    } else if is_class_definition(kind, language) {
        let class_name_node = node.child_by_field_name("name");
        if let Some(class_name_node) = class_name_node {
            let extracted_class_name = class_name_node
                .utf8_text(code.as_bytes())
                .unwrap()
                .to_string();

            if cursor.goto_first_child() {
                loop {
                    traverse_tree(
                        code,
                        cursor,
                        blocks,
                        non_function_blocks,
                        language,
                        Some(extracted_class_name.clone()),
                        module_name,
                        imports,
                        config,
                    );
                    if !cursor.goto_next_sibling() {
                        break;
                    }
                }
                cursor.goto_parent();
            }
        }
    } else if is_function_node(kind, language) {
        let function_name = get_function_name(code, node, language)
            .unwrap_or_else(|| "UnnamedFunction".to_string());
        let block_type = BlockType::Function;
        let block_content = node.utf8_text(code.as_bytes()).unwrap().to_string();

        let node_key = generate_node_key(
            Path::new(module_name),
            class_name.as_deref(),
            &function_name,
        );

        let mut block = Block::new(
            node_key,
            block_type,
            block_content,
            Some(function_name.clone()),
            class_name.clone(),
        );

        block.outgoing_calls = find_calls(code, node, language, module_name, imports);

        blocks.push(block);
    } else if !node.is_named() {
        let block_content = node.utf8_text(code.as_bytes()).unwrap().to_string();
        non_function_blocks.push(block_content);
    }

    if cursor.goto_first_child() {
        loop {
            traverse_tree(
                code,
                cursor,
                blocks,
                non_function_blocks,
                language,
                class_name.clone(),
                module_name,
                imports,
                &config,
            );
            if !cursor.goto_next_sibling() {
                break;
            }
        }
        cursor.goto_parent();
    }
}

fn find_calls(
    code: &str,
    root: Node,
    language: Language,
    module_name: &str,
    imports: &HashMap<String, String>,
) -> Vec<String> {
    let mut calls = HashSet::new();
    let mut cursor = root.walk();

    loop {
        let node = cursor.node();

        if is_call_expression(node.kind(), language) {
            if let Some(function_name) = get_call_expression_name(code, node, language) {
                let parts: Vec<&str> = function_name.split('.').collect();

                if parts.len() > 1 {
                    // This is for method calls on an object; the part before '.' is treated as an object, not a module.
                    let object_name = parts[0];
                    let method_name = &parts[1..].join(".");

                    // If the object name matches an alias from the imports, resolve to the correct module.
                    if let Some(imported_module) = imports.get(object_name) {
                        let call_key = generate_node_key(
                            Path::new(imported_module),
                            Some(object_name),
                            method_name,
                        );
                        calls.insert(call_key);
                    } else {
                        let call_key = generate_node_key(
                            Path::new(module_name),
                            Some(object_name),
                            method_name,
                        );
                        calls.insert(call_key);
                    }
                } else {
                    // For global function calls, check if the function name matches an alias from the imports.
                    if let Some(imported_module) = imports.get(&function_name) {
                        let call_key = generate_node_key(
                            Path::new(&format!("test-code-base/{}.py", imported_module)),
                            None,
                            &function_name,
                        );
                        calls.insert(call_key);
                    } else {
                        let function_key =
                            generate_node_key(Path::new(module_name), None, &function_name);
                        calls.insert(function_key);
                    }
                }
            }
        }

        if !cursor.goto_first_child() {
            while !cursor.goto_next_sibling() {
                if !cursor.goto_parent() {
                    return calls.into_iter().collect();
                }
            }
        }
    }
}

fn is_import_statement(kind: &str, language: Language) -> bool {
    match language {
        lang if lang == unsafe { tree_sitter_python() } => {
            kind == "import_statement" || kind == "import_from_statement"
        }
        lang if lang == unsafe { tree_sitter_rust() } => kind == "use_declaration",
        // Add more language-specific checks here
        _ => false,
    }
}

fn filter_import_matchers(
    child: Node,
    code: &str,
    matchers: &Matchers,
) -> (Option<String>, Option<String>, Option<String>) {
    let module = child
        .child_by_field_name(&matchers.module_name.field_name)
        .map(|n| {
            if n.kind() == matchers.module_name.kind {
                return n.utf8_text(code.as_bytes()).unwrap_or_default().to_owned();
            }

            String::default()
        });

    let name = child
        .child_by_field_name(&matchers.object_name.field_name)
        .map(|n| {
            if n.kind() == matchers.object_name.kind {
                return n.utf8_text(code.as_bytes()).unwrap_or_default().to_owned();
            }

            String::default()
        });

    let alias = child
        .child_by_field_name(&matchers.alias.field_name)
        .map(|n| {
            if n.kind() == matchers.alias.kind {
                return n.utf8_text(code.as_bytes()).unwrap_or_default().to_owned();
            }

            String::default()
        });

    (module, name, alias)
}

fn parse_import_statement(
    code: &str,
    node: Node,
    language: Language,
    config: &Config,
) -> Option<(String, String)> {
    let mut module_name = String::new();
    let mut object_name = String::new();
    let mut alias_name = String::new();

    match language {
        lang if lang == unsafe { tree_sitter_python() } => {
            let matchers = &config
            .languages
            .get("python")
            .expect("Failed to get Python matchers from config")
            .matchers;

            if node.kind() == matchers.import_statement {
                let result = filter_import_matchers(node, code, matchers);
                (module_name, object_name, alias_name) = (
                    result.0.unwrap_or(module_name),
                    result.1.unwrap_or(object_name),
                    result.2.unwrap_or(alias_name),
                );

                let mut cursor = node.walk();
                for child in node.named_children(&mut cursor) {
                    let result = filter_import_matchers(child, code, matchers);
                    (module_name, object_name, alias_name) = (
                        result.0.unwrap_or(module_name),
                        result.1.unwrap_or(object_name),
                        result.2.unwrap_or(alias_name),
                    );

                    let mut cursor2 = child.walk();
                    for child2 in child.named_children(&mut cursor2) {
                        let result = filter_import_matchers(child2, code, matchers);
                        (module_name, object_name, alias_name) = (
                            result.0.unwrap_or(module_name),
                            result.1.unwrap_or(object_name),
                            result.2.unwrap_or(alias_name),
                        );
                    }
                }

                println!(
                    "Module: {}, Object: {}, Alias: {}",
                    module_name, object_name, alias_name
                );
                return Some((module_name, object_name));
            }
            None
        },
        lang if lang == unsafe { tree_sitter_rust() } => {
            let matchers = &config
            .languages
            .get("rust")
            .expect("Failed to get Python matchers from config")
            .matchers;

            if node.kind() == matchers.import_statement {
                let result = filter_import_matchers(node, code, matchers);
                (module_name, object_name, alias_name) = (
                    result.0.unwrap_or(module_name),
                    result.1.unwrap_or(object_name),
                    result.2.unwrap_or(alias_name),
                );

                let mut cursor = node.walk();
                for child in node.named_children(&mut cursor) {
                    let result = filter_import_matchers(child, code, matchers);
                    (module_name, object_name, alias_name) = (
                        result.0.unwrap_or(module_name),
                        result.1.unwrap_or(object_name),
                        result.2.unwrap_or(alias_name),
                    );

                    let mut cursor2 = child.walk();
                    for child2 in child.named_children(&mut cursor2) {
                        let result = filter_import_matchers(child2, code, matchers);
                        (module_name, object_name, alias_name) = (
                            result.0.unwrap_or(module_name),
                            result.1.unwrap_or(object_name),
                            result.2.unwrap_or(alias_name),
                        );
                    }
                }

                println!(
                    "Module: {}, Object: {}, Alias: {}",
                    module_name, object_name, alias_name
                );
                return Some((module_name, object_name));
            }
            None
        }
        _ => None,
    }
}

fn is_class_definition(kind: &str, language: Language) -> bool {
    match language {
        lang if lang == unsafe { tree_sitter_python() } => kind == "class_definition",
        // Add more language-specific checks here
        _ => false,
    }
}

fn is_function_node(kind: &str, language: Language) -> bool {
    match language {
        lang if lang == unsafe { tree_sitter_rust() } => kind == "function_item",
        lang if lang == unsafe { tree_sitter_python() } => kind == "function_definition",
        // Add more language-specific checks here
        _ => false,
    }
}

fn get_function_name(code: &str, node: Node, language: Language) -> Option<String> {
    match language {
        lang if lang == unsafe { tree_sitter_rust() } => node
            .child_by_field_name("name")
            .and_then(|child| Some(child.utf8_text(code.as_bytes()).unwrap()))
            .map(|s| s.to_string()),
        lang if lang == unsafe { tree_sitter_python() } => node
            .child_by_field_name("name")
            .and_then(|child| Some(child.utf8_text(code.as_bytes()).unwrap()))
            .map(|s| s.to_string()),
        // Add more language-specific checks here
        _ => None,
    }
}

fn is_call_expression(kind: &str, language: Language) -> bool {
    match language {
        lang if lang == unsafe { tree_sitter_rust() } => kind == "call_expression",
        lang if lang == unsafe { tree_sitter_python() } => kind == "call",
        // Add more language-specific checks here
        _ => false,
    }
}

fn get_call_expression_name(code: &str, node: Node, language: Language) -> Option<String> {
    match language {
        lang if lang == unsafe { tree_sitter_rust() } => node
            .child_by_field_name("function")
            .and_then(|child| Some(child.utf8_text(code.as_bytes()).unwrap()))
            .map(|s| s.to_string()),
        lang if lang == unsafe { tree_sitter_python() } => node
            .child_by_field_name("function")
            .and_then(|child| Some(child.utf8_text(code.as_bytes()).unwrap()))
            .map(|s| s.to_string()),
        // Add more language-specific checks here
        _ => None,
    }
}
