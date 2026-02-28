(function_definition declarator: (function_declarator declarator: (_) @name)) @definition.function
(class_specifier name: (type_identifier) @name) @definition.class
(struct_specifier name: (type_identifier) @name) @definition.type
(enum_specifier name: (type_identifier) @name) @definition.type
(namespace_definition name: (namespace_identifier) @name) @definition.module
(call_expression function: (_) @name) @reference.call
[(identifier) (field_identifier) (type_identifier) (namespace_identifier)] @reference.identifier
(preproc_include) @import
