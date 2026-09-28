-- Image-relative animal locations. Historical classifications have no boxes.
ALTER TABLE classifications ADD COLUMN bounding_boxes_json TEXT;
