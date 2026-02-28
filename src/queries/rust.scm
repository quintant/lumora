; Functions
(function_item name: (identifier) @name) @definition.function

; Structs
(struct_item name: (type_identifier) @name) @definition.class

; Enums
(enum_item name: (type_identifier) @name) @definition.type

; Traits
(trait_item name: (type_identifier) @name) @definition.type

; Impl blocks
(impl_item trait: (type_identifier)? @name) @definition.class

; Modules
(mod_item name: (identifier) @name) @definition.module

; Const items
(const_item name: (identifier) @name) @definition.function

; Type aliases
(type_item name: (type_identifier) @name) @definition.type

; Macro definitions
(macro_definition name: (identifier) @name) @definition.function

; Call expressions
(call_expression function: (_) @name) @reference.call

; Identifier references
[(identifier) (type_identifier)] @reference.identifier

; Use declarations (imports)
(use_declaration) @import
