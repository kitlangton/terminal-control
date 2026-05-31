import { mkdir, mkdtemp, readFile, readdir, rm, writeFile } from "node:fs/promises"
import { tmpdir } from "node:os"
import { basename, join, resolve } from "node:path"
import { nativePackage, pack, packNativePackage, run, runtimeTargets } from "./native-packages.mjs"

const repository = resolve(import.meta.dirname, "..")
const packaged = process.argv[2] === "--tarballs"
const target = packaged ? process.argv[4] : runtimeTargets[`${process.platform}-${process.arch}`]
if (!target) throw new Error(`cannot validate native npm package for ${process.platform}-${process.arch}`)
const native = nativePackage(target)

const temp = await mkdtemp(join(tmpdir(), "cellshot-npm-validation-"))
try {
  const { clientTarball, nativeTarball } = await inputTarballs(temp)
  verifyDependencyVersion(clientTarball, nativeTarball)
  const installableClient = await clientWithLocalNative(temp, clientTarball, nativeTarball)
  await validateBunConsumer(join(temp, "bun-consumer"), installableClient)
  await validateNodeConsumer(join(temp, "node-consumer"), installableClient)
  console.log(`validated ${basename(clientTarball)} with automatic ${basename(nativeTarball)} install`)
} finally {
  if (!process.env.CELLSHOT_KEEP_NPM_VALIDATION_TEMP) {
    await rm(temp, { recursive: true, force: true })
  }
}

function verifyDependencyVersion(clientTarball, nativeTarball) {
  const client = archiveManifest(clientTarball)
  const packagedNative = archiveManifest(nativeTarball)
  const declared = client.optionalDependencies?.[native.name]
  if (declared !== packagedNative.version) {
    throw new Error(`${client.name} declares ${native.name}@${declared}, but tarball is ${packagedNative.version}`)
  }
}

function archiveManifest(tarball) {
  return JSON.parse(run("tar", ["-xOf", tarball, "package/package.json"], repository))
}

async function inputTarballs(temp) {
  if (packaged) {
    const tarballs = resolve(process.argv[3])
    const files = await readdir(tarballs)
    return {
      nativeTarball: join(tarballs, required(files, `cellshot-${target}-`)),
      clientTarball: join(tarballs, required(files, "cellshot-test-")),
    }
  }
  const tarballs = join(temp, "tarballs")
  await mkdir(tarballs, { recursive: true })
  return {
    nativeTarball: await packNativePackage(
      target,
      resolve(process.argv[2] ?? join(repository, "target/release/cellshot")),
      tarballs,
    ),
    clientTarball: pack(join(repository, "packages/test"), tarballs),
  }
}

async function clientWithLocalNative(temp, clientTarball, nativeTarball) {
  const staging = join(temp, "client-package")
  const output = join(temp, "installable")
  await mkdir(staging, { recursive: true })
  await mkdir(output, { recursive: true })
  run("tar", ["-xzf", clientTarball, "-C", staging], temp)
  const directory = join(staging, "package")
  const manifestPath = join(directory, "package.json")
  const manifest = JSON.parse(await readFile(manifestPath, "utf8"))
  if (!(native.name in manifest.optionalDependencies)) {
    throw new Error(`${manifest.name} does not declare ${native.name} as an optional dependency`)
  }
  manifest.optionalDependencies = { [native.name]: `file:${nativeTarball}` }
  await writeFile(manifestPath, `${JSON.stringify(manifest, null, 2)}\n`)
  return pack(directory, output, true)
}

function required(files, prefix) {
  const matches = files.filter((file) => file.startsWith(prefix) && file.endsWith(".tgz"))
  if (matches.length !== 1) throw new Error(`expected one ${prefix} tarball, found ${matches.length}`)
  return matches[0]
}

async function validateBunConsumer(directory, client) {
  await mkdir(directory, { recursive: true })
  await writeFile(join(directory, "package.json"), JSON.stringify({
    private: true,
    type: "module",
    dependencies: {
      "@cellshot/test": `file:${client}`,
    },
  }, null, 2))
  await writeFile(join(directory, "client.test.ts"), `import { expect, test } from "bun:test"\nimport { createCellshot } from "@cellshot/test"\ntest("resolves the installed native binary", async () => {\n  await using client = await createCellshot()\n  await using session = await client.launch({ command: ["sh", "-c", "printf bun-packed"] })\n  expect(await session.screen.text({ settleMs: 10, deadlineMs: 2_000 })).toBe("bun-packed")\n})\n`)
  run("bun", ["install"], directory)
  run("bun", ["test", "client.test.ts"], directory)
}

async function validateNodeConsumer(directory, client) {
  await mkdir(directory, { recursive: true })
  await writeFile(join(directory, "package.json"), JSON.stringify({
    private: true,
    type: "module",
    scripts: { test: "vitest run" },
    dependencies: {
      "@cellshot/test": `file:${client}`,
      "@types/node": "^24.0.0",
      typescript: "^5.9.0",
      vitest: "^3.2.4",
    },
  }, null, 2))
  await writeFile(join(directory, "tsconfig.json"), JSON.stringify({
    compilerOptions: {
      strict: true,
      noEmit: true,
      target: "ESNext",
      module: "NodeNext",
      moduleResolution: "NodeNext",
      skipLibCheck: true,
      types: ["node", "vitest"],
    },
  }, null, 2))
  await writeFile(join(directory, "client.test.ts"), `import { describe, expect, test } from "vitest"\nimport { createCellshot } from "@cellshot/test"\nimport { extendCellshotMatchers } from "@cellshot/test/vitest"\nextendCellshotMatchers(expect)\ndescribe("published package", () => {\n  test("resolves native binary and matcher", async () => {\n    await using client = await createCellshot()\n    await using session = await client.launch({ command: ["sh", "-c", "printf node-packed"] })\n    await expect(session).toHaveScreenText("node-packed")\n  })\n})\n`)
  run("npm", ["install", "--ignore-scripts"], directory)
  run("npm", ["exec", "--", "tsc", "--noEmit"], directory)
  run("npm", ["test"], directory)
}
