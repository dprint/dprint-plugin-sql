/**
 * This script checks for any sqlformat updates and then automatically
 * publishes a new version of the plugin if so.
 */
import { $, CargoToml, semver } from "automation";

const rootDirPath = $.path(import.meta.dirname!).parentOrThrow();
const cargoToml = new CargoToml(rootDirPath.join("Cargo.toml"));
const currentVersion = getSqlformatVersion(cargoToml.text());

$.logStep("Getting latest version...");
const latestVersion = await getLatestSqlformatVersion();
if (semver.compare(currentVersion, latestVersion) >= 0) {
  $.log("No new update found. Exiting.");
  Deno.exit(0);
}

$.log("Found new version.");
$.logStep("Updating Cargo.toml...");
const isPatchBump = currentVersion.major === latestVersion.major
  && currentVersion.minor === latestVersion.minor;
const currentVersionStr = `${currentVersion.major}.${currentVersion.minor}.${currentVersion.patch}`;
const latestVersionStr = `${latestVersion.major}.${latestVersion.minor}.${latestVersion.patch}`;
cargoToml.replaceAll(`sqlformat = "${currentVersionStr}"`, `sqlformat = "${latestVersionStr}"`);

// run the tests
$.logStep("Running tests...");
await $`cargo test`;

if (Deno.args.includes("--skip-publish")) {
  Deno.exit(0);
}

$.logStep(`Committing sqlformat version bump commit...`);
await $`git add .`;
const message = `${isPatchBump ? "fix" : "feat"}: update to sqlformat ${latestVersionStr}`;
await $`git commit -m ${message}`;

$.logStep("Bumping version in Cargo.toml...");
cargoToml.bumpCargoTomlVersion(isPatchBump ? "patch" : "minor");

// release
const newVersion = cargoToml.version();
$.logStep(`Committing and publishing ${newVersion}...`);
await $`git add .`;
await $`git commit -m ${newVersion}`;
await $`git push origin main`;
await $`git tag ${newVersion}`;
await $`git push origin ${newVersion}`;

function getSqlformatVersion(text: string) {
  const match = text.match(/sqlformat\s*=\s*"([^"]+)"/);
  const version = match?.[1];
  if (version == null) {
    throw new Error("Could not find sqlformat version in Cargo.toml.");
  }
  $.logLight("Found sqlformat version in Cargo.toml:", version);
  return semver.parse(version);
}

async function getLatestSqlformatVersion() {
  const response = await fetch("https://crates.io/api/v1/crates/sqlformat", {
    headers: {
      "User-Agent": "dprint-plugin-sql update script",
    },
  });
  const data = await response.json();
  const version = data.crate.max_stable_version ?? data.crate.max_version;
  $.logLight("Latest sqlformat version:", version);
  return semver.parse(version);
}
