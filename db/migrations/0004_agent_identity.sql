ALTER TABLE `sessions` ADD `agent_identity` text;--> statement-breakpoint
CREATE INDEX `sessions_by_agent_identity` ON `sessions` (`agent_identity`);
