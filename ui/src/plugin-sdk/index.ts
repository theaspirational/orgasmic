// The public SDK export list. Built alongside the host, so hooks and providers
// use the same React and transport instances. No P5 service placeholders.
export { fetchGraphNodes, fetchOrgNode, postOrgNodeEdit, postOrgNodeDelete } from '@/lib/api';
export type { NodeEditOp, OrgNodeDoc } from '@/lib/orgdoc/types';
export type { GraphNodeSummary, DaemonEvent } from '@/lib/types';
export { get, post } from '@/lib/transport';
export { useResource } from '@/lib/useResource';
export { useEventStream } from '@/hooks/useEventStream';
export { useMe } from '@/hooks/useMe';
export { useActiveProject } from '@/hooks/useActiveProject';
export { Button } from '@/components/ui/button';
export { Card, CardContent, CardDescription, CardFooter, CardHeader, CardTitle } from '@/components/ui/card';
export { Input } from '@/components/ui/input';
export { Textarea } from '@/components/ui/textarea';
