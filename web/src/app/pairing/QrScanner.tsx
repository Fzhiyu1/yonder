import { useEffect, useRef, useState } from 'react';
import { Camera } from 'lucide-react';
import { Spinner } from '../ui/primitives';

interface Detector {
  detect(src: CanvasImageSource): Promise<Array<{ rawValue: string }>>;
}

declare global {
  interface Window {
    BarcodeDetector?: new (opts: { formats: string[] }) => Detector;
  }
}

/** Camera QR scanner: BarcodeDetector when available, jsQR otherwise. */
export function QrScanner({ onResult }: { onResult: (text: string) => void }) {
  const video = useRef<HTMLVideoElement>(null);
  const [error, setError] = useState<string>();
  const [ready, setReady] = useState(false);
  const done = useRef(false);

  useEffect(() => {
    let stream: MediaStream | undefined;
    let raf = 0;
    let stopped = false;
    const canvas = document.createElement('canvas');
    const ctx = canvas.getContext('2d', { willReadFrequently: true });

    (async () => {
      if (!navigator.mediaDevices?.getUserMedia) {
        setError('此浏览器无法使用相机，请粘贴配对链接');
        return;
      }
      try {
        stream = await navigator.mediaDevices.getUserMedia({ video: { facingMode: 'environment' }, audio: false });
      } catch {
        setError('无法打开相机，请检查权限或粘贴配对链接');
        return;
      }
      if (stopped || !video.current) return;
      video.current.srcObject = stream;
      await video.current.play().catch(() => undefined);
      setReady(true);
      let detector: Detector | undefined;
      if (window.BarcodeDetector) {
        try {
          detector = new window.BarcodeDetector({ formats: ['qr_code'] });
        } catch {
          detector = undefined;
        }
      }
      const jsqr = detector ? undefined : (await import('jsqr')).default;
      const tick = async () => {
        if (stopped || done.current) return;
        const v = video.current;
        if (v && v.readyState >= 2 && v.videoWidth) {
          try {
            let text: string | undefined;
            if (detector) {
              const codes = await detector.detect(v);
              text = codes[0]?.rawValue;
            } else if (jsqr && ctx) {
              const scale = Math.min(1, 640 / v.videoWidth);
              canvas.width = Math.round(v.videoWidth * scale);
              canvas.height = Math.round(v.videoHeight * scale);
              ctx.drawImage(v, 0, 0, canvas.width, canvas.height);
              const img = ctx.getImageData(0, 0, canvas.width, canvas.height);
              text = jsqr(img.data, img.width, img.height, { inversionAttempts: 'dontInvert' })?.data;
            }
            if (text) {
              done.current = true;
              onResult(text);
              return;
            }
          } catch {
            /* keep scanning */
          }
        }
        raf = requestAnimationFrame(() => void tick());
      };
      void tick();
    })();

    return () => {
      stopped = true;
      cancelAnimationFrame(raf);
      stream?.getTracks().forEach((t) => t.stop());
    };
  }, [onResult]);

  return (
    <div className="relative aspect-square w-full overflow-hidden rounded-lg border border-line bg-black">
      <video ref={video} playsInline muted className="h-full w-full object-cover" />
      {!ready && !error && (
        <div className="absolute inset-0 flex items-center justify-center gap-2 text-sm text-white/80">
          <Spinner /> 正在打开相机
        </div>
      )}
      {error && (
        <div className="absolute inset-0 flex flex-col items-center justify-center gap-2 p-6 text-center text-sm text-white/80">
          <Camera size={22} />
          {error}
        </div>
      )}
      {ready && <div className="pointer-events-none absolute inset-[18%] rounded-lg border-2 border-white/70" />}
    </div>
  );
}
