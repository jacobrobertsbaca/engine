/** Cache storage is private to one Engine; it is never persisted or shared. */
export interface CppCacheLimits {
  maxObjectBytes: number;
  maxEntries: number;
  maxMetadataBytes: number;
  maxEntryMetadataBytes: number;
}

export const DEFAULT_CPP_CACHE_LIMITS: Readonly<CppCacheLimits> = Object.freeze({
  maxObjectBytes: 128 * 1024 * 1024,
  maxEntries: 256,
  maxMetadataBytes: 8 * 1024 * 1024,
  maxEntryMetadataBytes: 1024 * 1024
});

export interface ObjectRecord {
  source: string;
  key: string;
  object_hash: string;
  object: Uint8Array;
}

export interface ObjectCacheStats {
  objectBytes: number;
  metadataBytes: number;
  entries: number;
  evictions: number;
  oversized: number;
}

export class CppObjectCache {
  private records = new Map<string, ObjectRecord & { metadataBytes: number }>();
  private objectBytes = 0;
  private metadataBytes = 0;
  private evictions = 0;
  private oversized = 0;
  public readonly limits: Readonly<CppCacheLimits>;

  constructor(limits: Partial<CppCacheLimits> = {}) {
    this.limits = Object.freeze({ ...DEFAULT_CPP_CACHE_LIMITS, ...limits });
    for (const value of Object.values(this.limits))
      if (!Number.isSafeInteger(value) || value < 0)
        throw new RangeError('C++ cache limits must be nonnegative safe integers');
  }

  snapshot(): ObjectRecord[] {
    return [...this.records.values()].map(({ source, key, object_hash, object }) => ({
      source,
      key,
      object_hash,
      object: object.slice()
    }));
  }

  /** Worker hits are the only accesses that move an entry to the LRU tail. */
  touch(source: string): void {
    const record = this.records.get(source);
    if (record) {
      this.records.delete(source);
      this.records.set(source, record);
    }
  }

  put(value: ObjectRecord): void {
    // Treat unreadable records as misses. The worker also verifies object digest and schema.
    if (
      typeof value.source !== 'string' ||
      typeof value.key !== 'string' ||
      typeof value.object_hash !== 'string' ||
      !/^[a-f0-9]{64}$/.test(value.object_hash) ||
      !/^[a-f0-9]{64}$/.test(value.key) ||
      !(value.object instanceof Uint8Array)
    )
      return;
    const metadataBytes = new TextEncoder().encode(
      JSON.stringify({
        source: value.source,
        key: value.key,
        object_hash: value.object_hash
      })
    ).byteLength;
    const limits = this.limits;
    // Replacing an oversized record removes its stale predecessor, but never changes compilation.
    this.remove(value.source);
    if (
      limits.maxEntries === 0 ||
      value.object.byteLength > limits.maxObjectBytes ||
      metadataBytes > limits.maxMetadataBytes ||
      metadataBytes > limits.maxEntryMetadataBytes
    ) {
      this.oversized++;
      return;
    }
    while (
      this.records.size >= limits.maxEntries ||
      this.objectBytes + value.object.byteLength > limits.maxObjectBytes ||
      this.metadataBytes + metadataBytes > limits.maxMetadataBytes
    ) {
      this.remove(this.records.keys().next().value!);
      this.evictions++;
    }
    const record = { ...value, object: value.object.slice(), metadataBytes };
    this.records.set(value.source, record);
    this.objectBytes += record.object.byteLength;
    this.metadataBytes += metadataBytes;
  }

  delete(source: string): void {
    this.remove(source);
  }

  private remove(source: string): void {
    const previous = this.records.get(source);
    if (!previous) return;
    this.objectBytes -= previous.object.byteLength;
    this.metadataBytes -= previous.metadataBytes;
    this.records.delete(source);
  }

  clear(): void {
    this.records.clear();
    this.objectBytes = this.metadataBytes = 0;
  }

  stats(): ObjectCacheStats {
    return {
      objectBytes: this.objectBytes,
      metadataBytes: this.metadataBytes,
      entries: this.records.size,
      evictions: this.evictions,
      oversized: this.oversized
    };
  }
}
