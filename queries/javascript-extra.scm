;; lintent additions on top of the JavaScript tags query.

;; A class field initialised with a function is a method in all but syntax.
(field_definition
  property: (_) @name
  value: [(arrow_function) (function_expression)]) @definition.method

;; The upstream tags query skips constructors; they are worth judging too.
(method_definition
  name: (property_identifier) @name
  (#eq? @name "constructor")) @definition.method

;; A function passed to a call whose result is bound to a const is the body
;; of that const: `const loadFn = createServerFn().handler(async () => …)`,
;; `const onClick = useCallback(() => …, [])`. The whole declaration is the
;; scope, named `loadFn.handler` (lintent joins `@name` and `@name.member`).
;; Plain collection callbacks (`const xs = items.map(x => …)`) are excluded:
;; they belong to the function around them, not a scope of their own.
(variable_declarator
  name: (identifier) @name
  value: (call_expression
    function: (member_expression
      property: (property_identifier) @name.member)
    arguments: (arguments [(arrow_function) (function_expression)]))
  (#not-any-of? @name.member
    "map" "flatMap" "filter" "reduce" "reduceRight" "forEach" "find" "findIndex"
    "findLast" "findLastIndex" "some" "every" "sort" "toSorted" "then" "catch" "finally")) @definition.function

(variable_declarator
  name: (identifier) @name
  value: (call_expression
    function: (identifier)
    arguments: (arguments [(arrow_function) (function_expression)]))) @definition.function

;; `export default function () {}` / `export default () => …` have no name for
;; the upstream query to capture; lintent names them `default`.
(export_statement
  value: [(arrow_function) (function_expression)] @definition.function)

(export_statement
  declaration: (function_declaration
    !name) @definition.function)
