(method name: (_) @name) @definition.method
(singleton_method name: (_) @name) @definition.method
(class name: (_) @name) @definition.class
(module name: (_) @name) @definition.module
(call method: (_) @name) @reference.call
(identifier) @reference.identifier
(call method: (identifier) @name) @import
