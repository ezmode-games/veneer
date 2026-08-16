// Fixture component source beside quality-indicator.classes.ts. Declares its
// element through the forwardRef generic, the second of the two sources the
// resolver reads (issue #109).
export const QualityIndicator = React.forwardRef<HTMLSpanElement, QualityIndicatorProps>(
  (props, ref) => <span ref={ref} />,
);
