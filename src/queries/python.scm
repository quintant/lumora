; Functions
(function_definition name: (identifier) @name) @definition.function

; Classes
(class_definition name: (identifier) @name) @definition.class

; Calls
(call function: (_) @name) @reference.call

; Identifier references
(identifier) @reference.identifier

; Import statements
(import_statement) @import
(import_from_statement) @import
