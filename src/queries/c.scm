(function_definition declarator: (function_declarator declarator: (identifier) @name)) @definition.function
(struct_specifier name: (type_identifier) @name) @definition.type
(enum_specifier name: (type_identifier) @name) @definition.type
(call_expression function: (_) @name) @reference.call
[(identifier) (type_identifier)] @reference.identifier
(preproc_include) @import
