(function_definition name: (word) @name) @definition.function
(command name: (command_name (word) @name)) @reference.call
(variable_name) @reference.identifier
(command name: (command_name (word) @name) . (word) @import)
