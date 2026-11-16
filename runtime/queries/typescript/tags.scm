; See runtime/queries/ecma/README.md for more info.

; inherits: _typescript,ecma

(function_declaration
  name: (identifier) @name) @definition.function

(class_declaration
  name: (type_identifier) @name) @definition.class

(method_definition
  name: (property_identifier) @name) @definition.method

(enum_declaration
  name: (identifier) @name) @definition.enum

(type_alias_declaration
  name: (type_identifier) @name) @definition.type

; The API browser (this fork): names of exported declarations.
(export_statement
  declaration: (_
    name: (_) @public))
