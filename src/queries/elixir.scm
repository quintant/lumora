(call target: (identifier) @_keyword (arguments (alias) @name)) @definition.module
(call target: (identifier) @_keyword (arguments [(identifier) @name (call target: (identifier) @name) (binary_operator left: (call target: (identifier) @name))])) @definition.function
(call target: (identifier) @name) @reference.call
(call target: (dot right: (identifier) @name)) @reference.call
(identifier) @reference.identifier
(alias) @reference.identifier
