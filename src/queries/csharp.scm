(method_declaration name: (identifier) @name) @definition.method
(class_declaration name: (identifier) @name) @definition.class
(struct_declaration name: (identifier) @name) @definition.type
(interface_declaration name: (identifier) @name) @definition.type
(enum_declaration name: (identifier) @name) @definition.type
(invocation_expression function: (_) @name) @reference.call
(identifier) @reference.identifier
(using_directive) @import
