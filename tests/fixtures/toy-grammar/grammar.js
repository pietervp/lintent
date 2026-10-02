/**
 * A deliberately tiny language used to test lintent's runtime grammar loading
 * and its heuristic (no tags query) scope extraction.
 */
module.exports = grammar({
  name: 'toy',
  extras: $ => [/\s/, $.comment],
  rules: {
    source_file: $ => repeat(choice($.class_declaration, $.function_definition)),
    class_declaration: $ => seq('class', field('name', $.identifier), '{', repeat($.function_definition), '}'),
    function_definition: $ => seq('fn', field('name', $.identifier), '(', ')', $.block),
    block: $ => seq('{', repeat(choice($.statement, $.function_definition)), '}'),
    statement: $ => seq($.identifier, ';'),
    identifier: _ => /[a-zA-Z_][a-zA-Z0-9_]*/,
    comment: _ => token(seq('//', /.*/)),
  },
});
