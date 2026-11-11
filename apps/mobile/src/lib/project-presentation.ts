import type { Project } from '@waku/client';

/**
 * Mirrors `Project::PROJECTLESS_NAME` in `crates/waku-protocol/src/model.rs`
 * and the web client's copy. It identifies the sentinel project on the wire,
 * so it is never localized; display names come from the translator instead.
 */
export const PROJECTLESS_NAME = 'No project';

export function isProjectlessProject(project: Pick<Project, 'name'>): boolean {
  return project.name === PROJECTLESS_NAME;
}

export function projectDisplayName(
  project: Pick<Project, 'name'>,
  projectlessName: string,
): string {
  return isProjectlessProject(project) ? projectlessName : project.name;
}
