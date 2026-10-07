export type Listener<T> = (value: T) => void;

export class Emitter<T> {
  private listeners = new Set<Listener<T>>();

  on(fn: Listener<T>): () => void {
    this.listeners.add(fn);
    return () => {
      this.listeners.delete(fn);
    };
  }

  emit(value: T): void {
    for (const fn of [...this.listeners]) {
      try {
        fn(value);
      } catch (err) {
        console.error(err);
      }
    }
  }

  get size(): number {
    return this.listeners.size;
  }
}
