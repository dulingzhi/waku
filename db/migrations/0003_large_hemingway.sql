ALTER TABLE `sessions` ADD `parent_session_id` text;--> statement-breakpoint
CREATE INDEX `sessions_by_parent` ON `sessions` (`parent_session_id`);