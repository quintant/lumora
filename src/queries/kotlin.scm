(function_declaration (simple_identifier) @name) @definition.function
(class_declaration (type_identifier) @name) @definition.class
(object_declaration (type_identifier) @name) @definition.class
(type_alias (type_identifier) @name) @definition.type
(call_expression (simple_identifier) @name) @reference.call
(call_expression (navigation_expression (navigation_suffix (simple_identifier) @name))) @reference.call
[(simple_identifier) (type_identifier)] @reference.identifier
(import_header) @import
