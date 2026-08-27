import { cpSync, existsSync, mkdirSync, readFileSync, rmSync, statSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { pathToFileURL } from "node:url";

const PINNED_COMMIT = "e86823096c5bad39e1ca282ec24bc5eb9bec745b";
const FIXTURE_TIMESTAMP = 1_700_000_000_000;
const FIXTURE_API_KEY = "fixture-openrouter-image-key-never-forwarded";
const PNG_1X1 =
	"iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";

Date.now = () => FIXTURE_TIMESTAMP;

interface CapturedRequest {
	method: string;
	path: string;
	headers: Record<string, string>;
	body: Uint8Array;
}

interface ImageFixture {
	name: string;
	description: string;
	context: Record<string, unknown>;
	output: string[];
	response: Record<string, unknown>;
}

interface PublishedImageModel {
	id: string;
	name: string;
	input: string[];
	output: string[];
	cost: { input: number; output: number; cacheRead: number; cacheWrite: number };
}

function rawOpenRouterCatalogRecord(value: unknown): Record<string, unknown> {
	const model = value as PublishedImageModel;
	return {
		id: model.id,
		name: model.name,
		architecture: {
			input_modalities: model.input,
			output_modalities: model.output,
		},
		pricing: {
			prompt: String(model.cost.input / 1_000_000),
			completion: String(model.cost.output / 1_000_000),
			input_cache_read: String(model.cost.cacheRead / 1_000_000),
			input_cache_write: String(model.cost.cacheWrite / 1_000_000),
		},
	};
}

const fixtures: ImageFixture[] = [
	{
		name: "text-only",
		description: "Text prompt requesting image-only output.",
		context: { input: [{ type: "text", text: "Draw a deterministic blue square." }] },
		output: ["image"],
		response: {
			id: "generation-fixture-image-1",
			usage: {
				prompt_tokens: 12,
				completion_tokens: 8,
				total_tokens: 20,
				prompt_tokens_details: { cached_tokens: 0 },
			},
			choices: [{ message: { content: null, images: [{ image_url: { url: `data:image/png;base64,${PNG_1X1}` } }] } }],
		},
	},
	{
		name: "image-input",
		description: "Text and a base64 image are lowered as ordered user content.",
		context: {
			input: [
				{ type: "text", text: "Restyle this deterministic image." },
				{ type: "image", mimeType: "image/png", data: PNG_1X1 },
			],
		},
		output: ["image"],
		response: {
			id: "generation-fixture-image-2",
			choices: [{ message: { content: "", images: [{ image_url: `data:image/png;base64,${PNG_1X1}` }] } }],
		},
	},
	{
		name: "text-and-image-output",
		description: "A text-capable image model requests image and text modalities.",
		context: { input: [{ type: "text", text: "Draw and caption a deterministic blue square." }] },
		output: ["text", "image"],
		response: {
			id: "generation-fixture-image-3",
			choices: [{
				message: {
					content: "A blue square.",
					images: [
						{ image_url: { url: `data:image/png;base64,${PNG_1X1}` } },
						{ image_url: "https://example.invalid/not-embedded.png" },
					],
				},
			}],
		},
	},
];

function verifyPin(piRoot: string): void {
	const gitFile = join(piRoot, ".git");
	if (!existsSync(gitFile)) throw new Error(`PI checkout has no .git metadata: ${piRoot}`);
	const gitDirectory = statSync(gitFile).isDirectory()
		? gitFile
		: resolve(dirname(gitFile), readFileSync(gitFile, "utf8").trim().replace(/^gitdir:\s*/, ""));
	const head = readFileSync(join(gitDirectory, "HEAD"), "utf8").trim();
	const revision = head.startsWith("ref: ")
		? readFileSync(join(gitDirectory, head.slice(5)), "utf8").trim()
		: head;
	if (revision !== PINNED_COMMIT) {
		throw new Error(`Pi checkout must be ${PINNED_COMMIT}, found ${revision}`);
	}
}

function stableHeaders(headers: Headers): Record<string, string> {
	const omitted = new Set([
		"authorization",
		"content-length",
		"host",
		"user-agent",
	]);
	return Object.fromEntries(
		[...headers.entries()]
			.filter(([name]) => !omitted.has(name) && !name.startsWith("x-stainless-"))
			.sort(([left], [right]) => left.localeCompare(right)),
	);
}

function sha256(bytes: Uint8Array): string {
	return new Bun.CryptoHasher("sha256").update(bytes).digest("hex");
}

async function writeJson(path: string, value: unknown): Promise<void> {
	await Bun.write(path, `${JSON.stringify(value, null, 2)}\n`);
}

async function main(): Promise<void> {
	const toolRoot = import.meta.dir;
	const piRoot = resolve(process.env.PI_PIN_DIR ?? "/home/vikash/pi-pin-e86823096");
	verifyPin(piRoot);
	const workRoot = resolve(toolRoot, ".capture-work", "pi-ai-images");
	rmSync(workRoot, { recursive: true, force: true });
	mkdirSync(workRoot, { recursive: true });
	cpSync(join(piRoot, "packages", "ai", "src"), join(workRoot, "src"), { recursive: true });
	try {
		const apiModule = await import(
			`${pathToFileURL(join(workRoot, "src", "api", "openrouter-images.ts")).href}?pin=${PINNED_COMMIT}`
		);
		const catalogModule = await import(
			`${pathToFileURL(join(workRoot, "src", "image-models.generated.ts")).href}?pin=${PINNED_COMMIT}`
		);
		const publishedModels = Object.values(catalogModule.IMAGE_MODELS.openrouter as Record<string, unknown>);
		await writeJson(resolve(toolRoot, "..", "..", "agentprism-openrouter", "data", "image-models.json"), {
			data: publishedModels.map(rawOpenRouterCatalogRecord),
		});

		const outputRoot = resolve(toolRoot, "..", "openrouter-images");
		mkdirSync(outputRoot, { recursive: true });
		for (const fixture of fixtures) {
			const captured: CapturedRequest[] = [];
			const responseBytes = new TextEncoder().encode(JSON.stringify(fixture.response));
			const server = Bun.serve({
				port: 0,
				async fetch(request) {
					captured.push({
						method: request.method,
						path: new URL(request.url).pathname,
						headers: stableHeaders(request.headers),
						body: new Uint8Array(await request.arrayBuffer()),
					});
					return new Response(responseBytes, {
						status: 200,
						headers: { "content-type": "application/json", "x-request-id": "request-image-fixture-0001" },
					});
				},
			});
			try {
				const model = {
					id: `fixture/${fixture.name}`,
					name: `Fixture ${fixture.name}`,
					api: "openrouter-images",
					provider: "openrouter",
					baseUrl: `http://127.0.0.1:${server.port}/api/v1`,
					input: ["text", "image"],
					output: fixture.output,
					cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
				};
				const result = await apiModule.generateImages(model, structuredClone(fixture.context), {
					apiKey: FIXTURE_API_KEY,
					fetch: globalThis.fetch,
					maxRetries: 0,
					timeoutMs: 10_000,
				});
				if (result.stopReason !== "stop" || captured.length !== 1) {
					throw new Error(`${fixture.name} capture failed: ${result.errorMessage ?? "unexpected request count"}`);
				}
				const directory = resolve(outputRoot, fixture.name);
				rmSync(directory, { recursive: true, force: true });
				mkdirSync(directory, { recursive: true });
				const request = captured[0];
				const canonicalModel = { ...model, baseUrl: "http://127.0.0.1:<injected-port>/api/v1" };
				await writeJson(join(directory, "canonical.json"), {
					schemaVersion: 1,
					family: "openrouter-images",
					case: fixture.name,
					description: fixture.description,
					piCommit: PINNED_COMMIT,
					model: canonicalModel,
					context: fixture.context,
					options: { maxRetries: 0, timeoutMs: 10_000, apiKey: "[REDACTED]" },
					entrypoint: "generateImages",
					deterministicInjections: { timestamp: FIXTURE_TIMESTAMP },
				});
				await Bun.write(join(directory, "request.body.json"), request.body);
				await writeJson(join(directory, "request.headers.json"), {
					schemaVersion: 1,
					method: request.method,
					path: request.path,
					headers: request.headers,
					authentication: "[REDACTED]",
				});
				await Bun.write(join(directory, "response.body.json"), responseBytes);
				await writeJson(join(directory, "metadata.json"), {
					schemaVersion: 1,
					captureMode: "hermetic-local-server",
					credentialsUsed: false,
					credentialSource: null,
					secretsRedacted: true,
					requestSha256: sha256(request.body),
					responseSha256: sha256(responseBytes),
				});
				console.log(`captured openrouter-images/${fixture.name} (${PINNED_COMMIT})`);
			} finally {
				server.stop(true);
			}
		}
		const liveRequested = process.env.PI_FIXTURE_OPENROUTER_IMAGES_LIVE === "1";
		const liveKey = process.env.OPENROUTER_API_KEY;
		let liveAcceptance: Record<string, unknown> = {
			status: "not-run",
			reason: liveRequested ? "OPENROUTER_API_KEY is unavailable" : "live mode was not requested",
		};
		if (liveRequested && liveKey) {
			const modelId = process.env.PI_FIXTURE_OPENROUTER_IMAGE_MODEL ?? "google/gemini-2.5-flash-image";
			const model = publishedModels.find(
				(value) => (value as { id?: unknown }).id === modelId,
			) as Record<string, unknown> | undefined;
			if (!model) throw new Error(`live image model is not in the pinned catalog: ${modelId}`);
			const result = await apiModule.generateImages(model, {
				input: [{ type: "text", text: "Generate a simple solid blue square on a white background." }],
			}, {
				apiKey: liveKey,
				maxRetries: 0,
				timeoutMs: 180_000,
			});
			liveAcceptance = result.stopReason === "stop"
				? {
					status: "captured",
					model: modelId,
					responseId: result.responseId,
					stopReason: result.stopReason,
					output: result.output.map((item: Record<string, unknown>) => ({
						type: item.type,
						...(item.type === "image" ? { mimeType: item.mimeType } : {}),
					})),
				}
				: {
					status: "not-captured",
					model: modelId,
					stopReason: result.stopReason,
					reason: String(result.errorMessage ?? "provider returned no error message").replaceAll(liveKey, "[REDACTED]"),
				};
		}
		await writeJson(join(outputRoot, "live-acceptance.json"), {
			schemaVersion: 1,
			piCommit: PINNED_COMMIT,
			captureMode: "credential-backed-direct-acceptance",
			credentialSource: liveKey ? "OPENROUTER_API_KEY" : null,
			secretsPersisted: false,
			result: liveAcceptance,
		});
		await writeJson(join(outputRoot, "report.json"), {
			schemaVersion: 1,
			piCommit: PINNED_COMMIT,
			captureMode: "hermetic-local-server",
			credentialsAvailable: Boolean(process.env.OPENROUTER_API_KEY),
			credentialBackedAcceptance: liveAcceptance,
			results: fixtures.map((fixture) => ({ case: fixture.name, status: "captured" })),
		});
	} finally {
		rmSync(workRoot, { recursive: true, force: true });
	}
}

await main();
