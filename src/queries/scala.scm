(function_definition name: (identifier) @name) @definition.function
(class_definition name: (identifier) @name) @definition.class
(trait_definition name: (identifier) @name) @definition.type
(object_definition name: (identifier) @name) @definition.class
(enum_definition name: (identifier) @name) @definition.type
(type_definition name: (type_identifier) @name) @definition.type
(val_definition pattern: (identifier) @name) @definition.function
(var_definition pattern: (identifier) @name) @definition.function
(call_expression function: (identifier) @name) @reference.call
[(identifier) (type_identifier)] @reference.identifier
(import_declaration) @import
(package_clause name: (package_identifier) @name) @definition.module
