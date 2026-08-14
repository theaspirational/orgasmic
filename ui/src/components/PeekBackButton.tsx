import { ArrowLeft } from 'lucide-react';

import { Button } from '@/components/ui/button';

export function PeekBackButton({
  depth,
  onBack,
}: {
  depth: number;
  onBack: () => void;
}) {
  const remaining = Math.max(0, depth - 1);
  const label = remaining > 0 ? `Back to previous item (${remaining} more)` : 'Back and close';

  return (
    <>
      <Button
        type="button"
        variant="ghost"
        size="icon-sm"
        className="hidden shrink-0 sm:inline-flex"
        onClick={onBack}
        aria-label={label}
      >
        <ArrowLeft />
      </Button>
      <Button
        type="button"
        variant="ghost"
        size="sm"
        className="shrink-0 sm:hidden"
        onClick={onBack}
        aria-label={label}
      >
        <ArrowLeft />
        Back{remaining > 0 ? ` (${remaining})` : ''}
      </Button>
    </>
  );
}
