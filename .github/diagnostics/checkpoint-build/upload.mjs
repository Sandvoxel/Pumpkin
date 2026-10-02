import { readFile, writeFile, mkdir } from 'node:fs/promises';
import { createHash } from 'node:crypto';
import path from 'node:path';
import { DefaultArtifactClient } from '@actions/artifact';

// Runtime authorization stays inside the official client; arguments are paths only.
const request = JSON.parse(await readFile(process.argv[2], 'utf8'));
try {
  const client = new DefaultArtifactClient();
  const result = await client.uploadArtifact(request.name, [request.file],
    path.dirname(request.file), { retentionDays: 1, compressionLevel: 0 });
  if (!Number.isSafeInteger(result.id) || result.id <= 0) throw new Error('Missing artifact ID');
  if (request.verify) {
    await mkdir(request.download, { recursive: true });
    await client.downloadArtifact(result.id, { path: request.download });
    const restored = await readFile(path.join(request.download, path.basename(request.file)));
    if (createHash('sha256').update(restored).digest('hex') !== request.sha256)
      throw new Error('Round trip mismatch');
  }
  await writeFile(request.result, JSON.stringify({ id: result.id, size: result.size,
    digest: result.digest, verified: Boolean(request.verify) }) + '\n');
} catch {
  // SDK stdout/stderr are deliberately not relayed: errors may include signed URLs.
  await writeFile(request.result, JSON.stringify({ error: 'Artifact operation failed' }) + '\n');
  process.exitCode = 1;
}
