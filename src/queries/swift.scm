(function_declaration name: (simple_identifier) @name) @definition.function
(class_declaration name: (type_identifier) @name) @definition.class
(protocol_declaration name: (type_identifier) @name) @definition.type
(property_declaration (pattern (simple_identifier) @name)) @definition.function
(call_expression (simple_identifier) @name) @reference.call
[(simple_identifier) (type_identifier)] @reference.identifier
(import_declaration) @import
