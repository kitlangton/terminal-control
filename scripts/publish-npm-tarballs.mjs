import { readdir } from "node:fs/promises"
import { resolve } from "node:path"
import { spawnSync } from "node:child_process"
import { nativePackages } from "./native-packages.mjs"

const directory = resolve(process.argv[2] ?? "npm-artifacts")
const checkOnly = process.argv.includes("--check")
const files = (await readdir(directory)).filter((file) => file.endsWith(".tgz"))
const expected = ["@cellshot/test", ...nativePackages.map((entry) => entry.name)]
const tarballs = new Map()
for (const file of files) {
  const manifest = manifestOf(resolve(directory, file))
  if (!expected.includes(manifest.name)) {
    throw new Error(`unexpected npm tarball ${manifest.name} in ${directory}`)
  }
  if (tarballs.has(manifest.name)) {
    throw new Error(`duplicate npm tarball for ${manifest.name}`)
  }
  tarballs.set(manifest.name, { file, version: manifest.version })
}
for (const name of expected) {
  if (!tarballs.has(name)) throw new Error(`missing npm tarball for ${name}`)
}
const versions = new Set([...tarballs.values()].map(({ version }) => version))
if (versions.size !== 1) {
  throw new Error(`npm package versions are not aligned: ${[...versions].join(", ")}`)
}
for (const name of [...nativePackages.map((entry) => entry.name), "@cellshot/test"]) {
  if (!checkOnly) publish(resolve(directory, tarballs.get(name).file))
}
if (checkOnly) console.log(`validated complete npm release set at version ${[...versions][0]}`)

function manifestOf(tarball) {
  const result = spawnSync("tar", ["-xOf", tarball, "package/package.json"], { encoding: "utf8" })
  if (result.status !== 0) throw new Error(`read ${tarball}: ${result.stderr}`)
  return JSON.parse(result.stdout)
}

function publish(file) {
  const result = spawnSync("npm", ["publish", file, "--access", "public", "--provenance"], {
    stdio: "inherit",
  })
  if (result.status !== 0) process.exit(result.status ?? 1)
}
