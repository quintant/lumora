(function_declaration name: (identifier) @name) @definition.function
(method_declaration name: (field_identifier) @name) @definition.method
(type_declaration (type_spec name: (type_identifier) @name)) @definition.type
(call_expression function: (_) @name) @reference.call
[(identifier) (field_identifier) (type_identifier)] @reference.identifier
(import_declaration) @import
