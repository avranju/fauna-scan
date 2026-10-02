import fs from 'node:fs/promises';
import path from 'node:path';
import ts from 'typescript';

const check = process.argv.includes('--check');
let failed = false;

function isFunction(statement) {
  return (
    ts.isFunctionDeclaration(statement) ||
    (ts.isVariableStatement(statement) &&
      statement.declarationList.declarations.some(
        ({ initializer }) =>
          initializer &&
          (ts.isArrowFunction(initializer) ||
            ts.isFunctionExpression(initializer)),
      ))
  );
}

async function visit(directory) {
  for (const entry of await fs.readdir(directory, { withFileTypes: true })) {
    if (
      ['node_modules', 'dist', 'test-results', 'playwright-report'].includes(
        entry.name,
      )
    ) {
      continue;
    }
    const filename = path.join(directory, entry.name);
    if (entry.isDirectory()) {
      await visit(filename);
      continue;
    }
    if (!/\.tsx?$/.test(filename)) continue;
    const source = await fs.readFile(filename, 'utf8');
    const ast = ts.createSourceFile(
      filename,
      source,
      ts.ScriptTarget.Latest,
      true,
    );
    const edits = new Map();
    function inspect(node) {
      if (node.statements) {
        const statements = [...node.statements];
        for (let i = 1; i < statements.length; i++) {
          const before = statements[i - 1];
          const after = statements[i];
          const importBoundary =
            ts.isImportDeclaration(before) && !ts.isImportDeclaration(after);
          if (
            importBoundary ||
            isFunction(before) ||
            isFunction(after) ||
            (ts.isInterfaceDeclaration(before) &&
              ts.isInterfaceDeclaration(after))
          ) {
            const start = before.end;
            const end = after.getStart(ast);
            const gap = source.slice(start, end);
            if (/^\s*$/.test(gap)) {
              const indent = gap.slice(gap.lastIndexOf('\n') + 1);
              edits.set(start, { start, end, value: `\n\n${indent}` });
            }
          }
        }
      }
      ts.forEachChild(node, inspect);
    }
    inspect(ast);
    let formatted = source;
    for (const { start, end, value } of [...edits.values()].sort(
      (a, b) => b.start - a.start,
    )) {
      formatted = formatted.slice(0, start) + value + formatted.slice(end);
    }
    if (formatted !== source) {
      if (check) {
        console.error(`Missing function/import spacing: ${filename}`);
        failed = true;
      } else {
        await fs.writeFile(filename, formatted);
      }
    }
  }
}

await visit('.');
if (failed) process.exitCode = 1;
